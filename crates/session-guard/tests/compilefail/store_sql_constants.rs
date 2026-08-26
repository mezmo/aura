// The store.rs SQL constants must not be nameable outside the crate.

fn main() {
    let _ = session_guard::store::SCHEMA;
    let _ = session_guard::store::S1_CLAIM;
    let _ = session_guard::store::S1_CLASSIFY;
    let _ = session_guard::store::S2_HEARTBEAT;
    let _ = session_guard::store::S3_COMMIT;
    let _ = session_guard::store::S4_RELEASE;
    let _ = session_guard::store::S4_RELEASE_POD;
    let _ = session_guard::store::S5_RECONCILE;
    let _ = session_guard::store::S6_LOCATE;
}
