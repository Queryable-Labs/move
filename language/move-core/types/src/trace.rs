use crate::vm_status::VMStatus;
use serde::{Deserialize, Serialize};

#[cfg(any(test, feature = "fuzzing"))]
use proptest_derive::Arbitrary;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub enum CallType {
    Call,
    CallGeneric,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "fuzzing"), derive(Arbitrary))]
pub struct CallTrace {
    pub depth: u32,
    pub call_type: CallType,
    pub module_id: Option<String>,
    pub function: String,
    pub ty_args: Vec<Vec<u8>>,
    pub args_types: Vec<Vec<u8>>,
    pub args_values: Vec<Vec<u8>>,
    pub gas_used: u64,
    pub err: Option<VMStatus>,
}
