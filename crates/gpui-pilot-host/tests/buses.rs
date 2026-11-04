//! Every test process must be cut off from the desktop's D-Bus buses, whether it runs under
//! `make` or plain `cargo test`; the workspace `.cargo/config.toml` forces both addresses.

const NO_BUS: &str = "unix:path=/nonexistent/gpui-pilot-no-bus";

#[test]
fn test_processes_point_both_buses_at_nothing() {
    for name in ["DBUS_SYSTEM_BUS_ADDRESS", "DBUS_SESSION_BUS_ADDRESS"] {
        assert_eq!(std::env::var(name).as_deref(), Ok(NO_BUS), "{name}");
    }
}
