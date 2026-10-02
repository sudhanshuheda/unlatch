//! A "world" = a simulated Mac (fpsim) + a provider behind IPC + an agent on the VM side.
//! Scenarios and the fuzzer are written against [`World`] so they run unchanged against the
//! scripted engine ([`ScriptedWorld`]) and the real engine + `unlatchd` (`super::e2e::RealWorld`).

use super::backend::Backend;
use super::check::compare_trees;
use super::scripted::{Flaws, ReplyFault, ScriptedBackend, ScriptedEngine, ScriptedVm};
use super::sim::{FpSim, SimConfig};
use super::vmfs::{VmFs, VmTree};
use anyhow::Result;
use std::path::Path;
use std::time::Duration;

/// Default virtual-time budget of a quiesce: long enough for any backoff but the MQ-005 ceiling.
pub const QUIESCE_BUDGET: Duration = Duration::from_secs(3600);

pub trait World {
    type B: Backend;

    fn name(&self) -> String;
    fn sim(&mut self) -> &mut FpSim<Self::B>;
    fn vm(&mut self) -> &mut dyn VmFs;
    /// The engine has observed and committed every VM change made so far, and signalled it
    /// (`Engine::server_barrier` + `wait_idle`).
    fn settle(&mut self) -> Result<()>;
    /// Cut / restore the link to the VM. Restoring waits until the engine is live again.
    fn set_online(&mut self, online: bool) -> Result<()>;
    /// Restart the engine process (replica and journal persist).
    fn restart_engine(&mut self) -> Result<()>;
    /// Lose the reply of the next op of this kind at the IPC hop (`die_before_ipc_reply`).
    /// `Ok(false)` = this world cannot inject it (scenario is skipped).
    fn arm_reply_fault(&mut self, f: ReplyFault) -> Result<bool>;
    /// Whether ChangesSince can fail while offline in this world (MQ-005 needs failures).
    fn can_fail_enumerations(&self) -> bool {
        false
    }
    /// Whether [`World::settle`] answers a mass-deletion pause (rule 8) with "apply" by itself
    /// (default on: the fuzzer's agent really did delete that much). `Ok(false)` = this world
    /// cannot turn it off.
    fn set_auto_confirm(&mut self, _on: bool) -> Result<bool> {
        Ok(false)
    }
    /// The VM loses unlatchd's persisted index while the link is down (`index.bin` and its
    /// journal deleted; `reimage`: every bit of daemon state, as on a re-imaged VM): the next
    /// daemon builds a new index — new `IndexId`, every id renumbered. `Ok(false)` = this world
    /// cannot (scenario is skipped).
    fn wipe_daemon_index(&mut self, _reimage: bool) -> Result<bool> {
        Ok(false)
    }

    fn vm_tree(&mut self) -> Result<VmTree> {
        Ok(self.vm().snapshot()?)
    }

    /// Settle, then let fpsim run everything due (virtual time ≤ `QUIESCE_BUDGET`).
    fn sync(&mut self) -> Result<()> {
        self.settle()?;
        self.sim().pump(QUIESCE_BUDGET);
        self.settle()?;
        self.sim().pump(QUIESCE_BUDGET);
        Ok(())
    }

    /// Browse everything, sync, and return every invariant violation (empty = converged).
    fn converged(&mut self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        for _ in 0..3 {
            self.sync()?;
            for (p, e) in self.sim().browse_all() {
                problems.push(format!("browse {p}: {e}"));
            }
            if self.sim().pending() == 0 {
                break;
            }
        }
        self.sync()?;
        let tree = self.vm_tree()?;
        let sim = self.sim();
        problems.extend(compare_trees(&tree, &sim.visible()));
        problems.extend(
            sim.pending_report()
                .into_iter()
                .map(|p| format!("stuck pending: {p}")),
        );
        problems.extend(
            sim.disk()
                .duplicate_ids()
                .into_iter()
                .map(|id| format!("duplicate item id {id} on the Mac")),
        );
        problems.extend(
            sim.stats
                .violations
                .iter()
                .map(|v| format!("contract: {v}")),
        );
        Ok(problems)
    }
}

/// fpsim + [`ScriptedEngine`].
pub struct ScriptedWorld {
    pub engine: ScriptedEngine,
    sim: FpSim<ScriptedBackend>,
    vm: ScriptedVm,
    _scratch: tempfile::TempDir,
}

impl ScriptedWorld {
    pub fn new(flaws: Flaws) -> Result<ScriptedWorld> {
        Self::with_config(flaws, |_| {})
    }

    pub fn with_config(flaws: Flaws, tweak: impl FnOnce(&mut SimConfig)) -> Result<ScriptedWorld> {
        let scratch = tempfile::Builder::new().prefix("fpsim-").tempdir()?;
        let (engine, rx) = ScriptedEngine::new(flaws);
        let mut cfg = SimConfig::new("scripted", scratch.path());
        tweak(&mut cfg);
        let sim = FpSim::new(cfg, engine.backend(), rx);
        let vm = engine.vm();
        Ok(ScriptedWorld {
            engine,
            sim,
            vm,
            _scratch: scratch,
        })
    }

    pub fn scratch(&self) -> &Path {
        self._scratch.path()
    }
}

impl World for ScriptedWorld {
    type B = ScriptedBackend;

    fn name(&self) -> String {
        "scripted".into()
    }

    fn sim(&mut self) -> &mut FpSim<ScriptedBackend> {
        &mut self.sim
    }

    fn vm(&mut self) -> &mut dyn VmFs {
        &mut self.vm
    }

    fn settle(&mut self) -> Result<()> {
        self.engine.settle();
        Ok(())
    }

    fn set_online(&mut self, online: bool) -> Result<()> {
        self.engine.set_online(online);
        Ok(())
    }

    fn restart_engine(&mut self) -> Result<()> {
        self.engine.restart();
        Ok(())
    }

    fn arm_reply_fault(&mut self, f: ReplyFault) -> Result<bool> {
        self.engine.arm(f);
        Ok(true)
    }

    fn can_fail_enumerations(&self) -> bool {
        true
    }

    fn set_auto_confirm(&mut self, on: bool) -> Result<bool> {
        self.engine.set_auto_confirm(on);
        Ok(true)
    }
}
