//! fpsim over a real unix socket: `IpcFrame` postcard frames with content passed as file
//! descriptors (`SCM_RIGHTS`), a scripted engine behind a tiny IPC server, and a connection that
//! dies instead of replying (`die_before_ipc_reply`). The client reconnects like a freshly
//! launched extension (MQ-003) and replays with the same template id.

use std::time::Duration;
use unlatch_bench::fpsim::rawipc::{serve, RawIpcClient, RawServer};
use unlatch_bench::fpsim::scenarios::{run, SCENARIOS};
use unlatch_bench::fpsim::scripted::{Flaws, ReplyFault, ScriptedEngine, ScriptedVm};
use unlatch_bench::fpsim::sim::{FpSim, SimConfig};
use unlatch_bench::fpsim::vmfs::VmFs;
use unlatch_bench::fpsim::World;

/// fpsim ⇄ socket ⇄ scripted engine.
struct SocketWorld {
    engine: ScriptedEngine,
    sim: FpSim<RawIpcClient>,
    vm: ScriptedVm,
    _server: RawServer,
    _dir: tempfile::TempDir,
}

impl SocketWorld {
    fn new(flaws: Flaws) -> SocketWorld {
        let dir = tempfile::Builder::new()
            .prefix("fpsim-ipc-")
            .tempdir()
            .expect("tempdir");
        let (engine, rx) = ScriptedEngine::new(flaws);
        let sock = dir.path().join("engine.sock");
        let e2 = engine.clone();
        let server = serve(&sock, move || e2.backend()).expect("serve");
        let client = RawIpcClient::new(&sock, "socket", Duration::from_secs(10));
        let sim = FpSim::new(
            SimConfig::new("socket", &dir.path().join("scratch")),
            client,
            rx,
        );
        let vm = engine.vm();
        SocketWorld {
            engine,
            sim,
            vm,
            _server: server,
            _dir: dir,
        }
    }
}

impl World for SocketWorld {
    type B = RawIpcClient;

    fn name(&self) -> String {
        "socket".into()
    }

    fn sim(&mut self) -> &mut FpSim<RawIpcClient> {
        &mut self.sim
    }

    fn vm(&mut self) -> &mut dyn VmFs {
        &mut self.vm
    }

    fn settle(&mut self) -> anyhow::Result<()> {
        self.engine.settle();
        Ok(())
    }

    fn set_online(&mut self, online: bool) -> anyhow::Result<()> {
        self.engine.set_online(online);
        Ok(())
    }

    fn restart_engine(&mut self) -> anyhow::Result<()> {
        self.engine.restart();
        Ok(())
    }

    fn arm_reply_fault(&mut self, f: ReplyFault) -> anyhow::Result<bool> {
        self.engine.arm(f);
        Ok(true)
    }

    fn can_fail_enumerations(&self) -> bool {
        true
    }

    fn set_auto_confirm(&mut self, on: bool) -> anyhow::Result<bool> {
        self.engine.set_auto_confirm(on);
        Ok(true)
    }
}

#[test]
fn content_crosses_the_socket_as_file_descriptors() {
    let mut w = SocketWorld::new(Flaws::default());
    w.vm().mkdir("d").expect("mkdir");
    let big: Vec<u8> = (0..(1 << 20)).map(|i| (i % 251) as u8).collect();
    w.vm().write("d/big.bin", &big).expect("write");
    w.settle().expect("settle");
    w.sim().add_domain().expect("add");
    w.sim().browse("d").expect("browse");
    assert_eq!(
        w.sim().open("d/big.bin").expect("fetch over the socket"),
        big
    );
    let edit: Vec<u8> = big.iter().rev().copied().collect();
    w.sim().write("d/big.bin", &edit).expect("edit");
    w.sim()
        .create_file("d", "new.bin", &big[..4096])
        .expect("create");
    w.sync().expect("sync");
    let tree = w.vm_tree().expect("tree");
    assert_eq!(
        tree.nodes["d/big.bin"].content.as_deref(),
        Some(edit.as_slice())
    );
    assert_eq!(
        tree.nodes["d/new.bin"].content.as_deref(),
        Some(&big[..4096])
    );
    assert!(w.converged().expect("converged").is_empty());
}

#[test]
fn dropped_reply_reconnects_and_replays_once() {
    let mut w = SocketWorld::new(Flaws::default());
    w.sim().add_domain().expect("add");
    w.engine.arm(ReplyFault::Create);
    w.sim()
        .create_file("", "once.txt", b"exactly once")
        .expect("create");
    w.sync().expect("sync");
    assert!(
        w.sim().stats.transport_errors >= 1,
        "the injected fault closed the connection"
    );
    let tree = w.vm_tree().expect("tree");
    let copies = tree
        .contents()
        .filter(|(_, c)| c.as_slice() == b"exactly once")
        .count();
    assert_eq!(copies, 1, "{:?}", tree.nodes.keys().collect::<Vec<_>>());
    assert!(w.converged().expect("converged").is_empty());
}

#[test]
fn every_scenario_also_passes_over_the_socket() {
    for s in SCENARIOS {
        let mut base = Flaws::default();
        (s.base)(&mut base);
        let mut w = SocketWorld::new(base);
        let v = run(s.name, &mut w).unwrap_or_else(|e| panic!("{}: {e:#}", s.name));
        assert!(
            v.pass(),
            "{} over the socket: {:?} {:#?}",
            s.name,
            v.skipped,
            v.problems
        );
    }
}
