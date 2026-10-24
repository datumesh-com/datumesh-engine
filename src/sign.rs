//! The JSON text of a signed receipt.

use datumesh_core::json::{self, Value};
use datumesh_core::Receipt;

/// The canonical JSON text of a whole receipt: its payload (canonical, without `sig`) with
/// `sig` added.
pub(crate) fn receipt_json(r: &Receipt) -> Vec<u8> {
    let mut members = vec![("sig".to_string(), Value::String(r.sig().to_string()))];
    if let Ok(Value::Object(m)) = json::parse(r.payload()) {
        members.extend(m);
    }
    Value::Object(members).to_jcs()
}
