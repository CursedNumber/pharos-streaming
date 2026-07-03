// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

/**
 * Keeps the [UsbExportEngine] running as a foreground service so enrolled USB
 * devices keep forwarding to the Pharos host across screen changes — including
 * while a game is streaming. Auto-started at app launch (see
 * [MainActivity], which calls [ensureRunning]) and whenever a device is enabled;
 * stops itself when nothing is left enabled.
 */
class UsbExportService : Service() {
    private lateinit var engine: UsbExportEngine

    override fun onCreate() {
        super.onCreate()
        createChannel()
        engine = UsbExportEngine.get(this)
        // When the last device is disabled (or unplugged-and-disabled), wind down.
        engine.onIdle = { stopSelf() }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForegroundCompat()
        engine.startLoop()
        return START_STICKY
    }

    override fun onDestroy() {
        engine.onIdle = null
        engine.stopLoop()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun startForegroundCompat() {
        val notif = buildNotification()
        if (Build.VERSION.SDK_INT >= 30) {
            startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        } else {
            startForeground(NOTIF_ID, notif)
        }
    }

    private fun buildNotification(): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, UsbExportActivity::class.java),
            if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_IMMUTABLE else 0,
        )
        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("Trireme USB sharing")
            .setContentText("Forwarding controllers to the host")
            .setSmallIcon(android.R.drawable.stat_sys_data_bluetooth)
            .setOngoing(true)
            .setContentIntent(open)
            .setPriority(NotificationCompat.PRIORITY_LOW)
            .build()
    }

    private fun createChannel() {
        if (Build.VERSION.SDK_INT >= 26) {
            val mgr = getSystemService(NotificationManager::class.java)
            if (mgr.getNotificationChannel(CHANNEL_ID) == null) {
                mgr.createNotificationChannel(
                    NotificationChannel(CHANNEL_ID, "USB sharing", NotificationManager.IMPORTANCE_LOW)
                        .apply { description = "Active while Trireme forwards USB devices to the host" },
                )
            }
        }
    }

    companion object {
        private const val CHANNEL_ID = "usb_export"
        private const val NOTIF_ID = 4434

        /** Start the service if USB/IP is on AND any device is enabled for export.
         *  Safe to call from a foreground context (an Activity); no-op when the
         *  master switch is off or nothing is enabled. */
        fun ensureRunning(context: Context) {
            val store = UsbStore(context)
            if (!store.usbIpEnabled()) return // master off — never forward
            if (!store.anyEnabled()) return
            ContextCompat.startForegroundService(context, Intent(context, UsbExportService::class.java))
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, UsbExportService::class.java))
        }
    }
}
