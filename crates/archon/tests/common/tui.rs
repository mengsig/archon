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
    Reply::Orchestrator(OrchestratorInfo {
        agent: AetherAgentId::parse("agt_selected").unwrap(),
        model: ModelRef::new("codex", "gpt-6.1-sol"),
        effort: Effort::High,
        status: "ready".into(),
        restarts: 0,
        generation: 1,
        freshness: None,
    })
}

pub fn serve(
    socket: &Path,
    connections: usize,
    replies: Vec<Result<Reply, ErrorReply>>,
) -> JoinHandle<Vec<Request>> {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
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
                    Request::OrchestratorAttach => replies.next().expect("a scripted attach reply"),
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
        assert!(replies.next().is_none(), "all replies consumed");
        requests
    })
}
