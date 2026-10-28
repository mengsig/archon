//! A canned archond for launcher tests; no models or daemon autostart.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread::JoinHandle;
use std::time::Duration;

use archon_core::aether_ids::AetherAgentId;
use archon_core::model::{Effort, ModelRef};
use archon_core::protocol::{
    ClientFrame, ErrorCode, ErrorReply, OrchestratorInfo, PROTOCOL_VERSION, Reply, Request,
    ServerFrame, Welcome,
};

pub fn orchestrator() -> Reply {
    orchestrator_in("agt_selected", "ready")
}

/// The orchestrator `agent` in Aether state `status`.
pub fn orchestrator_in(agent: &str, status: &str) -> Reply {
    Reply::Orchestrator(OrchestratorInfo {
        agent: AetherAgentId::parse(agent).unwrap(),
        model: ModelRef::new("codex", "gpt-6.1-sol"),
        effort: Effort::High,
        status: status.into(),
        restarts: 0,
        generation: 1,
        freshness: None,
    })
}

/// `orchestrator_attach` and `orchestrator_status` take the scripted
/// `replies` in turn; all of them must be asked for.
pub fn serve(
    socket: &Path,
    connections: usize,
    replies: Vec<Result<Reply, ErrorReply>>,
) -> JoinHandle<Vec<Request>> {
    let mut replies = replies.into_iter();
    serve_with(socket, connections, move |request| match request {
        Some(request) => replies
            .next()
            .unwrap_or_else(|| panic!("no scripted reply for {request:?}")),
        None => {
            assert!(replies.next().is_none(), "all replies consumed");
            Ok(Reply::Ok)
        }
    })
}

/// As [`serve`], with `answer` replying to the orchestrator requests; it is
/// called once more with `None` when the last connection closed.
pub fn serve_with(
    socket: &Path,
    connections: usize,
    mut answer: impl FnMut(Option<&Request>) -> Result<Reply, ErrorReply> + Send + 'static,
) -> JoinHandle<Vec<Request>> {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..connections {
            let (mut writer, _) = listener.accept().unwrap();
            writer
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            for line in BufReader::new(writer.try_clone().unwrap()).lines() {
                let frame: ClientFrame = serde_json::from_str(&line.unwrap()).unwrap();
                let result = match &frame.request {
                    Request::Hello {
                        protocol_version, ..
                    } => {
                        assert_eq!(*protocol_version, PROTOCOL_VERSION);
                        Ok(Reply::Welcome(Welcome {
                            protocol_version: PROTOCOL_VERSION,
                            daemon_version: "test".into(),
                            pid: std::process::id(),
                        }))
                    }
                    Request::OrchestratorAttach | Request::OrchestratorStatus => {
                        answer(Some(&frame.request))
                    }
                    Request::Status(_) => Err(ErrorReply {
                        code: ErrorCode::Internal,
                        message: "no status in this canned archond".into(),
                    }),
                    other => panic!("unexpected launcher request {other:?}"),
                };
                requests.push(frame.request);
                let reply = ServerFrame::Reply {
                    id: frame.id,
                    result,
                };
                writeln!(writer, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
            }
        }
        let _ = answer(None);
        requests
    })
}
