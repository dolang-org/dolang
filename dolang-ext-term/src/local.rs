use std::cell::Cell;

use dolang::runtime::{Strand, strand};

pub(crate) struct Local {
    /// Set while dispatching into a console.
    ///
    /// A console written in Do may itself call `echo`; without this guard that
    /// would route straight back into the same console and recurse until the
    /// call-depth limit. While set, console writes bypass the capture and go to
    /// the host.
    capturing: Cell<bool>,
    /// The `can_style` the ambient console reported when it was installed.
    ///
    /// Snapshotted rather than read live because `can_style` is defined to be
    /// fixed for the life of an installed console — which is what makes a
    /// capture's styling deterministic — and because the styling query is a
    /// sync, infallible one reachable from a public Rust API.
    ///
    /// Only meaningful while a capture is installed; the host answers from
    /// what it supplied on installation instead.
    capture_can_style: Cell<bool>,
}

impl<'v> strand::Local<'v> for Local {
    fn init() -> Self {
        Self {
            capturing: Cell::new(false),
            capture_can_style: Cell::new(false),
        }
    }

    fn inherit(&self, _strand: &Strand<'v, '_>, _kind: strand::InheritKind) -> Self {
        Self {
            // Inherited so that a strand spawned from inside a console's own
            // write stays guarded rather than routing back into it.
            capturing: Cell::new(self.capturing.get()),
            // Inherited alongside the capture root itself, so a strand spawned
            // inside a capture answers the styling question the same way.
            capture_can_style: Cell::new(self.capture_can_style.get()),
        }
    }
}

impl Local {
    pub(crate) fn capturing(&self) -> bool {
        self.capturing.get()
    }

    pub(crate) fn set_capturing(&self, v: bool) -> bool {
        self.capturing.replace(v)
    }

    pub(crate) fn capture_can_style(&self) -> bool {
        self.capture_can_style.get()
    }

    pub(crate) fn set_capture_can_style(&self, v: bool) -> bool {
        self.capture_can_style.replace(v)
    }
}
