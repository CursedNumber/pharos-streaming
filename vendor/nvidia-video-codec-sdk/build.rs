//! Nothing to link: safe/api.rs loads the two NvEncodeAPI bootstrap symbols
//! from the driver's library at runtime (libnvidia-encode.so.1 /
//! nvEncodeAPI64.dll), and nothing in this workspace uses NVDEC. The binary
//! therefore starts on a machine without the NVIDIA driver and can report that
//! clearly, instead of dying in the dynamic loader. (The upstream build script
//! searched for the SDK's import libraries and emitted link directives.)

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
