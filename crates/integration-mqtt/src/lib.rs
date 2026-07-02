//! MQTT integration for Pharos/Trireme → Home Assistant.
//!
//! Publishes **HA MQTT-Discovery** entities (so they auto-appear in Home
//! Assistant — no YAML) plus a retained **availability** topic backed by an MQTT
//! **LWT** (last will), and dispatches inbound command topics to registered
//! handlers (for buttons/numbers/switches).
//!
//! Design: one background thread owns the `rumqttc` connection event loop; on
//! every (re)connect it republishes availability=online, all discovery configs,
//! and the last-known state of every entity, then (re)subscribes the command
//! wildcard. The host/client just calls [`Mqtt::announce`] / [`Mqtt::publish_state`]
//! / [`Mqtt::register_command`] — all thread-safe and reconnect-safe.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rumqttc::{Client, Event, LastWill, MqttOptions, Packet, QoS};

/// Host identity helpers (hostname + NIC MACs/IPs) for telemetry, Wake-on-LAN,
/// and MAC-driven socket binding. The implementation now lives in the lean
/// `host-info` leaf crate; this re-export keeps `integration_mqtt::host::*`
/// resolving for existing callers unchanged.
pub mod host {
    pub use host_info::*;
}

/// HA device-registry block — groups all entities under one device card.
#[derive(Clone)]
pub struct Device {
    pub identifier: String,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub sw_version: String,
}

/// Broker connection + this node's identity.
#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// MQTT client id (unique per running instance).
    pub client_id: String,
    /// HA discovery prefix (default `homeassistant`).
    pub discovery_prefix: String,
    /// Per-node topic root, e.g. `pharos/<id>` — state + command topics hang off it.
    pub base_topic: String,
    /// Stable node id used in unique_ids + the device identifier.
    pub node_id: String,
}

/// HA MQTT component type for an entity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Sensor,
    BinarySensor,
    Button,
    Number,
    Switch,
}

impl Component {
    fn ha_str(self) -> &'static str {
        match self {
            Component::Sensor => "sensor",
            Component::BinarySensor => "binary_sensor",
            Component::Button => "button",
            Component::Number => "number",
            Component::Switch => "switch",
        }
    }
    fn has_state(self) -> bool {
        !matches!(self, Component::Button)
    }
    fn has_command(self) -> bool {
        matches!(
            self,
            Component::Button | Component::Number | Component::Switch
        )
    }
}

/// One entity to publish via discovery. Build with [`EntitySpec::new`] + setters.
#[derive(Clone)]
pub struct EntitySpec {
    pub object_id: String,
    pub name: String,
    pub component: Component,
    pub device_class: Option<String>,
    pub unit: Option<String>,
    pub icon: Option<String>,
    pub state_class: Option<String>,
    /// "diagnostic" / "config" — keeps telemetry out of the main controls.
    pub entity_category: Option<String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
}

impl EntitySpec {
    pub fn new(
        object_id: impl Into<String>,
        name: impl Into<String>,
        component: Component,
    ) -> Self {
        Self {
            object_id: object_id.into(),
            name: name.into(),
            component,
            device_class: None,
            unit: None,
            icon: None,
            state_class: None,
            entity_category: None,
            min: None,
            max: None,
            step: None,
        }
    }
    pub fn sensor(object_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self::new(object_id, name, Component::Sensor)
    }
    pub fn binary_sensor(object_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self::new(object_id, name, Component::BinarySensor)
    }
    pub fn button(object_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self::new(object_id, name, Component::Button)
    }
    pub fn number(object_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self::new(object_id, name, Component::Number)
    }
    pub fn with_unit(mut self, u: impl Into<String>) -> Self {
        self.unit = Some(u.into());
        self
    }
    pub fn with_icon(mut self, i: impl Into<String>) -> Self {
        self.icon = Some(i.into());
        self
    }
    pub fn with_device_class(mut self, c: impl Into<String>) -> Self {
        self.device_class = Some(c.into());
        self
    }
    pub fn with_state_class(mut self, c: impl Into<String>) -> Self {
        self.state_class = Some(c.into());
        self
    }
    pub fn diagnostic(mut self) -> Self {
        self.entity_category = Some("diagnostic".into());
        self
    }
    pub fn range(mut self, min: f64, max: f64, step: f64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self.step = Some(step);
        self
    }
}

type CommandHandler = Arc<dyn Fn(String) + Send + Sync>;

struct StoredEntity {
    discovery_topic: String,
    discovery_payload: String,
}

#[derive(Default)]
struct Inner {
    entities: Vec<StoredEntity>,
    states: HashMap<String, String>, // object_id -> last value (for reconnect republish)
    commands: HashMap<String, CommandHandler>,
}

/// A live MQTT integration. Cheap to clone the publish handle via the inner
/// `Client`; the connection runs on its own thread until this is dropped.
pub struct Mqtt {
    client: Client,
    inner: Arc<Mutex<Inner>>,
    cfg: Config,
    device: Device,
    avail_topic: String,
    _handle: std::thread::JoinHandle<()>,
}

impl Mqtt {
    /// Connect to the broker (non-blocking — the event loop runs on a thread).
    /// Availability flips to `online` on connect and (via LWT) `offline` if the
    /// process dies; call [`Mqtt::announce`] for each entity afterwards.
    pub fn connect(cfg: Config, device: Device) -> Result<Self> {
        let avail_topic = format!("{}/availability", cfg.base_topic);

        let mut opts = MqttOptions::new(cfg.client_id.clone(), cfg.host.clone(), cfg.port);
        opts.set_keep_alive(Duration::from_secs(15));
        if let (Some(u), Some(p)) = (cfg.username.clone(), cfg.password.clone()) {
            opts.set_credentials(u, p);
        }
        opts.set_last_will(LastWill::new(
            avail_topic.clone(),
            "offline",
            QoS::AtLeastOnce,
            true,
        ));

        let (client, mut connection) = Client::new(opts, 64);
        let inner = Arc::new(Mutex::new(Inner::default()));

        // Event-loop thread: republish everything on each (re)connect; route
        // inbound command publishes to handlers.
        let t_client = client.clone();
        let t_inner = inner.clone();
        let avail = avail_topic.clone();
        let base = cfg.base_topic.clone();
        let device_dbg = device.name.clone();
        let handle = std::thread::Builder::new()
            .name("mqtt-eventloop".into())
            .spawn(move || {
                for notification in connection.iter() {
                    match notification {
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            tracing::info!(
                                "MQTT connected ({device_dbg}); (re)publishing discovery"
                            );
                            // online (retained)
                            let _ = t_client.publish(
                                avail.as_str(),
                                QoS::AtLeastOnce,
                                true,
                                "online".as_bytes(),
                            );
                            // subscribe the command wildcard
                            let _ = t_client.subscribe(format!("{base}/+/set"), QoS::AtLeastOnce);
                            // re-announce discovery + last states
                            if let Ok(g) = t_inner.lock() {
                                for e in &g.entities {
                                    let _ = t_client.publish(
                                        e.discovery_topic.as_str(),
                                        QoS::AtLeastOnce,
                                        true,
                                        e.discovery_payload.as_bytes(),
                                    );
                                }
                                for (obj, val) in &g.states {
                                    let _ = t_client.publish(
                                        format!("{base}/{obj}"),
                                        QoS::AtLeastOnce,
                                        true,
                                        val.as_bytes(),
                                    );
                                }
                            }
                        }
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            // topic = <base>/<object_id>/set
                            let obj = p
                                .topic
                                .strip_prefix(&format!("{base}/"))
                                .and_then(|s| s.strip_suffix("/set"))
                                .map(str::to_string);
                            if let Some(obj) = obj {
                                let handler = t_inner
                                    .lock()
                                    .ok()
                                    .and_then(|g| g.commands.get(&obj).cloned());
                                if let Some(h) = handler {
                                    let payload = String::from_utf8_lossy(&p.payload).to_string();
                                    tracing::info!("MQTT command {obj} = {payload}");
                                    h(payload);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            // rumqttc's iterator auto-reconnects; just pace + log.
                            tracing::debug!("MQTT event loop: {e}");
                            std::thread::sleep(Duration::from_secs(2));
                        }
                    }
                }
            })
            .context("spawn mqtt-eventloop thread")?;

        Ok(Self {
            client,
            inner,
            cfg,
            device,
            avail_topic,
            _handle: handle,
        })
    }

    /// Register + publish an entity's HA discovery config (retained). Idempotent;
    /// stored so it's re-sent on reconnect.
    pub fn announce(&self, spec: &EntitySpec) -> Result<()> {
        let discovery_topic = format!(
            "{}/{}/{}/{}/config",
            self.cfg.discovery_prefix,
            spec.component.ha_str(),
            self.cfg.node_id,
            spec.object_id
        );
        let payload = self.discovery_json(spec);
        let payload = serde_json::to_string(&payload).context("serialize discovery")?;
        if let Ok(mut g) = self.inner.lock() {
            g.entities.push(StoredEntity {
                discovery_topic: discovery_topic.clone(),
                discovery_payload: payload.clone(),
            });
        }
        self.client
            .publish(
                discovery_topic,
                QoS::AtLeastOnce,
                true,
                payload.into_bytes(),
            )
            .context("publish discovery")?;
        Ok(())
    }

    /// Publish (retained) the current value of a state-bearing entity.
    pub fn publish_state(&self, object_id: &str, value: impl Into<String>) {
        let value = value.into();
        if let Ok(mut g) = self.inner.lock() {
            g.states.insert(object_id.to_string(), value.clone());
        }
        let _ = self.client.publish(
            format!("{}/{}", self.cfg.base_topic, object_id),
            QoS::AtLeastOnce,
            true,
            value.into_bytes(),
        );
    }

    /// Register a handler for a command entity (button/number/switch). Called
    /// with the inbound payload whenever HA writes its `…/set` topic.
    pub fn register_command<F>(&self, object_id: &str, handler: F)
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        if let Ok(mut g) = self.inner.lock() {
            g.commands.insert(object_id.to_string(), Arc::new(handler));
        }
        // The wildcard subscription (set on connect) already covers this topic.
    }

    fn discovery_json(&self, spec: &EntitySpec) -> serde_json::Value {
        use serde_json::json;
        let mut m = serde_json::Map::new();
        m.insert("name".into(), json!(spec.name));
        m.insert(
            "unique_id".into(),
            json!(format!("{}_{}", self.cfg.node_id, spec.object_id)),
        );
        m.insert("availability_topic".into(), json!(self.avail_topic));
        m.insert(
            "device".into(),
            json!({
                "identifiers": [self.cfg.node_id],
                "name": self.dev_name(),
                "manufacturer": self.dev_manu(),
                "model": self.dev_model(),
                "sw_version": self.dev_sw(),
            }),
        );
        let state_topic = format!("{}/{}", self.cfg.base_topic, spec.object_id);
        let cmd_topic = format!("{}/{}/set", self.cfg.base_topic, spec.object_id);
        if spec.component.has_state() {
            m.insert("state_topic".into(), json!(state_topic));
        }
        if spec.component.has_command() {
            m.insert("command_topic".into(), json!(cmd_topic));
        }
        if let Some(v) = &spec.device_class {
            m.insert("device_class".into(), json!(v));
        }
        if let Some(v) = &spec.unit {
            m.insert("unit_of_measurement".into(), json!(v));
        }
        if let Some(v) = &spec.icon {
            m.insert("icon".into(), json!(v));
        }
        if let Some(v) = &spec.state_class {
            m.insert("state_class".into(), json!(v));
        }
        if let Some(v) = &spec.entity_category {
            m.insert("entity_category".into(), json!(v));
        }
        if let Some(v) = spec.min {
            m.insert("min".into(), json!(v));
        }
        if let Some(v) = spec.max {
            m.insert("max".into(), json!(v));
        }
        if let Some(v) = spec.step {
            m.insert("step".into(), json!(v));
        }
        serde_json::Value::Object(m)
    }

    // Device fields are stashed into the JSON at announce time; held here so the
    // discovery payload always carries the device card.
    fn dev_name(&self) -> &str {
        &self.device.name
    }
    fn dev_manu(&self) -> &str {
        &self.device.manufacturer
    }
    fn dev_model(&self) -> &str {
        &self.device.model
    }
    fn dev_sw(&self) -> &str {
        &self.device.sw_version
    }
}

impl Drop for Mqtt {
    fn drop(&mut self) {
        // Best-effort graceful offline (the LWT covers ungraceful exits).
        let _ = self.client.publish(
            self.avail_topic.as_str(),
            QoS::AtLeastOnce,
            true,
            "offline".as_bytes(),
        );
        let _ = self.client.disconnect();
    }
}
