//! Collector for the continuously-drained bounded child output tails.

use crate::pipeline::traits::Collector;
use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};
use crate::platform::ChildOutputCapture;
use mach2::port::mach_port_t;
use std::sync::Arc;
use std::time::Duration;

pub struct ProcessOutputCollector {
    capture: Arc<ChildOutputCapture>,
}

impl ProcessOutputCollector {
    #[must_use]
    pub fn new(capture: Arc<ChildOutputCapture>) -> Self {
        Self { capture }
    }
}

impl Plugin for ProcessOutputCollector {
    fn name(&self) -> &'static str {
        "ProcessOutputCollector"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl Collector for ProcessOutputCollector {
    fn collect(
        &self,
        event: &CrashEvent,
        _task: mach_port_t,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        data.raw.process_output = Some(if event.termination.is_some() {
            self.capture.snapshot_after_eof(Duration::from_millis(250))
        } else {
            self.capture.snapshot()
        });
        context.checkpoint()?;
        Ok(())
    }
}
