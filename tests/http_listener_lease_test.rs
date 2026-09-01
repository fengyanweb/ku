use std::{net::TcpListener, sync::Mutex};

use ku::cli::run_source;

static HTTP_LISTENER_LEASE_TEST_LOCK: Mutex<()> = Mutex::new(());

fn unused_local_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind temporary listener");
    let address = listener
        .local_addr()
        .expect("read temporary listener address")
        .to_string();
    drop(listener);
    address
}

#[test]
fn interpreter_listener_scope_drop_releases_bound_address() {
    let _guard = HTTP_LISTENER_LEASE_TEST_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let address = unused_local_address();
    let source = format!(
        r#"
import "std.http"

fn bind_and_drop(address: str): null! {{
    app = http.service()
    listener = app.bind(address)?
    if (listener.kind != "http.listener") {{
        panic("bad listener")
    }}
    return ok(null)
}}

fn main(): null! {{
    bind_and_drop("{address}")?
    app = http.service()
    listener = app.bind("{address}")?
    listener.close()?
    return ok(null)
}}
"#
    );

    run_source("http-listener-scope-drop.ku", &source)
        .expect("dropping the last listener value should release its socket");
}

#[test]
fn interpreter_listener_clone_does_not_close_original() {
    let _guard = HTTP_LISTENER_LEASE_TEST_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let address = unused_local_address();
    let source = format!(
        r#"
import "std.http"

fn main(): null! {{
    app = http.service()
    listener = app.bind("{address}")?
    if (true) {{
        copy = listener.clone()
        if (copy.address != "{address}") {{
            panic("bad cloned listener")
        }}
    }}
    listener.close()?
    return ok(null)
}}
"#
    );

    run_source("http-listener-clone.ku", &source)
        .expect("dropping a temporary listener clone must not close the original");
}

#[test]
fn interpreter_listener_explicit_close_and_lease_drop_are_idempotent() {
    let _guard = HTTP_LISTENER_LEASE_TEST_LOCK
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let address = unused_local_address();
    let source = format!(
        r#"
import "std.http"

fn main(): null! {{
    app = http.service()
    listener = app.bind("{address}")?
    listener.close()?
    try {{
        listener.close()?
        panic("second close should fail")
    }} catch (err) {{
        if (err.code != "close_failed") {{
            panic("bad close error")
        }}
    }}
    return ok(null)
}}
"#
    );

    run_source("http-listener-explicit-close.ku", &source)
        .expect("lease finalization after explicit close must be a no-op");
    TcpListener::bind(&address).expect("explicit close should release the bound address");
}
