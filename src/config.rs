use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub interface: String,
    pub device_name: String,
    pub tx_channels: u32,
    pub rx_channels: u32,
    pub sample_rate: u32,
    pub latency_ns: u32,
    /// Path where statime will export its usrvclock socket
    pub clock_path: String,
    /// Path for statime observation socket (PTP status monitoring)
    pub observation_path: String,
    /// Path to the statime binary
    pub statime_bin: PathBuf,
    /// Path to libasound_module_pcm_inferno.so
    pub inferno_so: PathBuf,
    /// Whether to set up a persistent PipeWire node
    pub use_pipewire: bool,
}

impl Default for Config {
    fn default() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let hostname = std::fs::read_to_string("/etc/hostname")
            .unwrap_or_else(|_| "linux".to_string())
            .trim()
            .to_string();

        Self {
            interface: String::new(),
            device_name: format!("{}-dante", hostname),
            tx_channels: 2,
            rx_channels: 2,
            sample_rate: 48000,
            latency_ns: 10_000_000,
            clock_path: "/tmp/dante-clock".to_string(),
            observation_path: "/tmp/idvs-statime-obs.sock".to_string(),
            statime_bin: exe_dir.join("statime"),
            inferno_so: exe_dir.join("libasound_module_pcm_inferno.so"),
            use_pipewire: true,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        if path.exists() {
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Ok(config) = serde_json::from_str::<Config>(&data) {
                    return config;
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, data);
        }
    }

    /// PipeWire quantum derived from the configured latency and sample rate.
    /// Rounded down to the nearest power of two so it fits within the Dante
    /// network latency budget while staying compatible with audio drivers.
    pub fn pipewire_quantum(&self) -> u32 {
        let frames = self.latency_ns as u64 * self.sample_rate as u64 / 1_000_000_000;
        let frames = frames.max(1) as u32;
        // largest power-of-two ≤ frames
        1u32 << (31 - frames.leading_zeros())
    }

    /// Generate the statime TOML configuration content
    pub fn statime_config(&self) -> String {
        format!(
            r#"loglevel = "info"
sdo-id = 0
domain = 0
priority1 = 251
virtual-system-clock = true
virtual-system-clock-base = "monotonic_raw"
usrvclock-export = true
usrvclock-path = "{clock_path}"

[[port]]
interface = "{interface}"
network-mode = "ipv4"
protocol-version = "PTPv1"

[observability]
observation-path = "{obs_path}"
observation-permissions = 0o666
"#,
            clock_path = self.clock_path,
            interface = self.interface,
            obs_path = self.observation_path,
        )
    }

    /// Generate the ALSA device configuration for ~/.asoundrc.
    /// The plugin .so is expected to be installed in the system ALSA lib dir
    /// (installed by idvs on startup via pkexec). ALSA finds it automatically
    /// by the naming convention libasound_module_pcm_inferno.so.
    pub fn alsa_config(&self, bind_ip: &str) -> String {
        format!(
            r#"pcm.dante {{
    type inferno
    NAME "{name}"
    BIND_IP "{bind_ip}"
    SAMPLE_RATE {rate}
    TX_CHANNELS {tx}
    RX_CHANNELS {rx}
    CLOCK_PATH "{clock}"
    TX_LATENCY_NS {lat}
    RX_LATENCY_NS {lat}

    hint {{
        show on
        description "Inferno Dante Virtual Soundcard"
    }}
}}

pcm.dante_plug {{
    type plug
    slave.pcm "dante"

    hint {{
        show on
        description "Inferno Dante Virtual Soundcard (plug)"
    }}
}}

ctl.dante {{
    type inferno
}}
"#,
            name = self.device_name,
            rate = self.sample_rate,
            tx = self.tx_channels,
            rx = self.rx_channels,
            clock = self.clock_path,
            lat = self.latency_ns,
        )
    }

    /// Build the pw-cli command to create a persistent PipeWire node (for reference/debugging)
    #[allow(dead_code)]
    pub fn pipewire_node_cmd(&self) -> Vec<String> {
        let alsa_path = format!(
            "dante:TX_CHANNELS={tx},RX_CHANNELS={rx}",
            tx = self.tx_channels,
            rx = self.rx_channels,
        );
        vec![
            "pw-cli".to_string(),
            "create-node".to_string(),
            "adapter".to_string(),
            format!(
                "{{ object.linger=1 factory.name=api.alsa.pcm.duplex node.name=\"{}\" media.class=Audio/Duplex api.alsa.path=\"{}\" session.suspend-timeout-seconds=0 node.pause-on-idle=false node.suspend-on-idle=false node.always-process=true api.alsa.headroom=0 }}",
                self.device_name,
                alsa_path,
            ),
        ]
    }
}

fn config_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config")
        });
    config_dir.join("idvs").join("config.json")
}
