//! #3642 P0 regression test: TUI `:restart` must delegate to daemon
//! `restart_instance` lifecycle; TUI must never locally spawn a backend for a
//! daemon-managed instance; single instance identity under one live backend.

use syn::visit::Visit;

struct RestartArmVisitor {
    in_restart_arm: bool,
    found_restart_instance: bool,
    found_create_remote_pane: bool,
    found_create_pane_from_resolved: bool,
    found_create_pane: bool,
    found_kill_agent: bool,
    found_spawn_agent: bool,
}

impl<'ast> Visit<'ast> for RestartArmVisitor {
    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        // Look for the "restart" pattern arm
        let is_restart = matches!(
            &arm.pat,
            syn::Pat::Lit(syn::PatLit {
                lit: syn::Lit::Str(s),
                ..
            }) if s.value() == "restart"
        );

        if is_restart {
            let prev = self.in_restart_arm;
            self.in_restart_arm = true;
            syn::visit::visit_arm(self, arm);
            self.in_restart_arm = prev;
        } else {
            syn::visit::visit_arm(self, arm);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if self.in_restart_arm {
            let path_str = path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");

            if path_str.contains("restart_instance") {
                self.found_restart_instance = true;
            }
            if path_str.contains("create_remote_pane") {
                self.found_create_remote_pane = true;
            }
            if path_str.contains("create_pane_from_resolved") {
                self.found_create_pane_from_resolved = true;
            }
            if path_str.ends_with("create_pane") {
                self.found_create_pane = true;
            }
            if path_str.contains("kill_agent") {
                self.found_kill_agent = true;
            }
            if path_str.contains("spawn_agent") {
                self.found_spawn_agent = true;
            }
        }
        syn::visit::visit_path(self, path);
    }
}

#[test]
fn restart_command_arm_strictly_delegates_to_daemon_and_never_spawns_locally_3642() {
    let source = include_str!("../src/app/commands.rs");
    let syntax: syn::File = syn::parse_str(source).expect("parse commands.rs");

    let mut visitor = RestartArmVisitor {
        in_restart_arm: false,
        found_restart_instance: false,
        found_create_remote_pane: false,
        found_create_pane_from_resolved: false,
        found_create_pane: false,
        found_kill_agent: false,
        found_spawn_agent: false,
    };

    visitor.visit_file(&syntax);

    assert!(
        visitor.found_restart_instance,
        ":restart arm must call daemon restart_instance RPC (#3642)"
    );
    assert!(
        visitor.found_create_remote_pane,
        ":restart arm must attach remote pane via bridge (#3642)"
    );
    assert!(
        !visitor.found_create_pane_from_resolved,
        ":restart arm must NOT call create_pane_from_resolved (#3642)"
    );
    assert!(
        !visitor.found_create_pane,
        ":restart arm must NOT call create_pane (#3642)"
    );
    assert!(
        !visitor.found_kill_agent,
        ":restart arm must NOT call kill_agent directly (#3642)"
    );
    assert!(
        !visitor.found_spawn_agent,
        ":restart arm must NOT call spawn_agent (#3642)"
    );
}
