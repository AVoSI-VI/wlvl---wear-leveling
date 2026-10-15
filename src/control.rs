//! Control socket: lets `wlvl status` query the running daemon.
//!
//! Protocol: one JSON request line, one JSON response line, then close.
//! Requests: `{"cmd":"list"}` or `{"cmd":"status","uuid":"...","hardware":true}`.

use crate::health::{HealthParams, StatusReport};
use crate::mapper::ChunkMapper;
use crate::ondisk::Layout;
use crate::persist::FlushSignal;
use crate::wear::WearStats;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Default socket for the multi-volume daemon.
pub const DEFAULT_SOCKET: &str = "/run/wlvl/control.sock";

/// Socket used by a stand-alone `wlvl attach` process.
pub fn per_volume_socket(uuid: Uuid) -> PathBuf {
    PathBuf::from(format!("/run/wlvl/{uuid}.sock"))
}

/// One attached volume as seen by the control plane.
pub struct LiveVolume {
    pub uuid: Uuid,
    pub device: PathBuf,
    pub bdev: Mutex<Option<String>>,
    pub layout: Layout,
    pub mapper: Arc<ChunkMapper>,
    pub wear_stats: Option<Arc<Mutex<WearStats>>>,
    /// Carries the generation of the newest durable commit.
    pub flush_signal: Arc<FlushSignal>,
    pub health_params: HealthParams,
}

impl LiveVolume {
    pub fn status(&self, with_hardware: bool) -> StatusReport {
        let wear = self
            .wear_stats
            .as_ref()
            .map(|w| w.lock().unwrap_or_else(|e| e.into_inner()).clone());
        let bdev = self.bdev.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let generation = self.flush_signal.generation();
        StatusReport::live(
            self.uuid,
            &self.device,
            bdev,
            &self.mapper,
            generation,
            wear,
            &self.health_params,
            with_hardware,
        )
    }
}

#[derive(Default)]
pub struct Registry {
    volumes: Mutex<Vec<Arc<LiveVolume>>>,
}

impl Registry {
    pub fn add(&self, v: Arc<LiveVolume>) {
        self.volumes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(v);
    }

    pub fn remove(&self, uuid: Uuid) {
        self.volumes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|v| v.uuid != uuid);
    }

    pub fn get(&self, uuid: Uuid) -> Option<Arc<LiveVolume>> {
        self.volumes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|v| v.uuid == uuid)
            .cloned()
    }

    pub fn list(&self) -> Vec<Arc<LiveVolume>> {
        self.volumes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    List,
    Status {
        uuid: Uuid,
        #[serde(default)]
        hardware: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VolumeSummary {
    pub uuid: Uuid,
    pub device: PathBuf,
    pub bdev: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    List { volumes: Vec<VolumeSummary> },
    Status { status: Box<StatusReport> },
    Error { message: String },
}

/// Compute the response for a request against the registry (transport independent).
pub fn handle(registry: &Registry, req: Request) -> Response {
    match req {
        Request::List => Response::List {
            volumes: registry
                .list()
                .iter()
                .map(|v| VolumeSummary {
                    uuid: v.uuid,
                    device: v.device.clone(),
                    bdev: v.bdev.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                })
                .collect(),
        },
        Request::Status { uuid, hardware } => match registry.get(uuid) {
            Some(v) => Response::Status {
                status: Box::new(v.status(hardware)),
            },
            None => Response::Error {
                message: format!("volume {uuid} is not attached here"),
            },
        },
    }
}

#[cfg(unix)]
pub use unix::{query, serve, ControlHandle};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    pub struct ControlHandle {
        path: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl ControlHandle {
        pub fn shutdown(mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Poke the listener so accept() returns.
            let _ = UnixStream::connect(&self.path);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    impl Drop for ControlHandle {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&self.path);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Bind `path` and serve requests on a background thread.
    pub fn serve(path: &Path, registry: Arc<Registry>) -> std::io::Result<ControlHandle> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A stale socket from a crashed daemon is replaced; a live one refuses to bind twice
        // only if we can connect to it.
        if path.exists() {
            if UnixStream::connect(path).is_ok() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("{} is already served", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("wlvl-control".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    if stop_t.load(Ordering::SeqCst) {
                        break;
                    }
                    match conn {
                        Ok(stream) => {
                            if let Err(e) = handle_conn(stream, &registry) {
                                tracing::debug!(error = %e, "control connection failed");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "control accept failed"),
                    }
                }
            })?;
        Ok(ControlHandle {
            path: path.to_path_buf(),
            stop,
            thread: Some(thread),
        })
    }

    fn handle_conn(mut stream: UnixStream, registry: &Registry) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut line = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut line)?;
        let resp = match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) => handle(registry, req),
            Err(e) => Response::Error {
                message: format!("bad request: {e}"),
            },
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        stream.write_all(out.as_bytes())
    }

    /// Client side: send one request and read the response.
    pub fn query(path: &Path, req: &Request) -> std::io::Result<Response> {
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        let mut resp = String::new();
        BufReader::new(stream).read_line(&mut resp)?;
        serde_json::from_str(resp.trim())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::layout::MIB;
    use crate::ondisk::Metadata;

    fn live(uuid: Uuid) -> Arc<LiveVolume> {
        let layout = Layout::compute(128 * MIB, 4 * MIB, 2, 10).unwrap();
        let mapper = Arc::new(ChunkMapper::new(layout, Metadata::initial(&layout)));
        Arc::new(LiveVolume {
            uuid,
            device: PathBuf::from("/dev/null"),
            bdev: Mutex::new(Some("/dev/ublkb0".into())),
            layout,
            mapper,
            wear_stats: Some(Arc::new(Mutex::new(WearStats::default()))),
            flush_signal: {
                let s = Arc::new(FlushSignal::default());
                s.set_generation(7);
                s
            },
            health_params: HealthParams::default(),
        })
    }

    #[test]
    fn handle_list_and_status() {
        let reg = Registry::default();
        let uuid = Uuid::new_v4();
        reg.add(live(uuid));
        match handle(&reg, Request::List) {
            Response::List { volumes } => assert_eq!((volumes.len(), volumes[0].uuid), (1, uuid)),
            other => panic!("unexpected {other:?}"),
        }
        match handle(
            &reg,
            Request::Status {
                uuid,
                hardware: false,
            },
        ) {
            Response::Status { status } => {
                assert!(status.live && status.hardware.is_none());
                assert_eq!(status.generation, 7);
                assert_eq!(status.health.device_score, 100);
                // Round-trips through JSON for the client.
                let json = serde_json::to_string(&*status).unwrap();
                let back: StatusReport = serde_json::from_str(&json).unwrap();
                assert_eq!(back, *status);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            handle(
                &reg,
                Request::Status {
                    uuid: Uuid::new_v4(),
                    hardware: false
                }
            ),
            Response::Error { .. }
        ));
        reg.remove(uuid);
        assert!(reg.list().is_empty());
    }

    #[test]
    fn request_wire_format() {
        let r: Request = serde_json::from_str(
            r#"{"cmd":"status","uuid":"6ba7b810-9dad-11d1-80b4-00c04fd430c8"}"#,
        )
        .unwrap();
        assert!(matches!(
            r,
            Request::Status {
                hardware: false,
                ..
            }
        ));
        let r: Request = serde_json::from_str(r#"{"cmd":"list"}"#).unwrap();
        assert!(matches!(r, Request::List));
    }
}
