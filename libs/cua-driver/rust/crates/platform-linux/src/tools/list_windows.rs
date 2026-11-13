// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

use async_trait::async_trait;
use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef},
};
use serde_json::{json, Value};

use super::impl_::window_record_json;
use crate::x11::WindowInfo;

/// Apply the tool's filters, then rank the surviving stacked windows `0..n`
/// in their stacking order. Backends number before these filters (KWin and
/// the shell helper report raw stacking values), so ranking first would leave
/// gaps.
fn retain_listed(
    windows: &mut Vec<WindowInfo>,
    on_screen_only: bool,
    app: Option<&str>,
    is_live: impl Fn(u32) -> bool,
) {
    // Exited applications can remain visible in AT-SPI/X11 as zombies;
    // never return those stale targets to callers.
    windows.retain(|window| window.pid.is_none_or(&is_live));
    if on_screen_only {
        windows.retain(|window| window.is_on_screen);
    }
    if let Some(filter) = app {
        windows.retain(|window| {
            cua_driver_core::tool_args::app_filter_matches(filter, &window.app_name, None)
        });
    }
    let mut stacked: Vec<usize> = windows.iter().filter_map(|w| w.z_index).collect();
    stacked.sort_unstable();
    stacked.dedup();
    for window in windows {
        window.z_index = window.z_index.and_then(|z| stacked.binary_search(&z).ok());
    }
}

fn window_list_line(w: &WindowInfo) -> String {
    let bounds = if w.is_on_screen && w.width > 0 && w.height > 0 {
        format!("{}x{}+{}+{}", w.width, w.height, w.x, w.y)
    } else {
        "hidden".to_owned()
    };
    format!(
        "  window_id={} pid={} \"{}\" {}{}",
        w.xid,
        w.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
        w.title,
        bounds,
        if w.app_name.is_empty() {
            String::new()
        } else {
            format!(" app={}", w.app_name)
        }
    )
}

// ── list_windows ─────────────────────────────────────────────────────────────

pub struct ListWindowsTool;
static LIST_WINDOWS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

#[async_trait]
impl Tool for ListWindowsTool {
    fn def(&self) -> &ToolDef {
        LIST_WINDOWS_DEF.get_or_init(|| ToolDef {
            name: "list_windows".into(),
            description: "List top-level windows. Filter with `app` or `pid`; an unfiltered call \
                lists every window on the host. Hidden windows are shown as `hidden` with no \
                bounds; focus them before acting. z_index in the structured record is stacking \
                order: higher is front, null means unavailable.".into(),
            input_schema: json!({"type":"object","properties":{
                "pid":{"type":"integer","description":"Only list windows owned by this process ID."},
                "app":{"type":"string","description":cua_driver_core::tool_args::LIST_WINDOWS_APP_FILTER_DESCRIPTION},
                "on_screen_only":{"type":"boolean","description":"When true, filter to visible windows only. Default false."}
            },"additionalProperties":false}),
            read_only: true, destructive: false, idempotent: true, open_world: false,
        })
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let filter_pid = args.opt_u64("pid").map(|v| v as u32);
        let on_screen_only = args.bool_or("on_screen_only", false);
        let mut windows =
            tokio::task::spawn_blocking(move || crate::wayland::list_windows_dispatch(filter_pid))
                .await
                .unwrap_or_default();
        let none_listed = windows.is_empty();
        retain_listed(
            &mut windows,
            on_screen_only,
            args.opt_str("app").as_deref(),
            crate::proc_fs::is_process_live,
        );
        if crate::wayland::is_wayland() {
            // The registry stores compositor-layout origins for click
            // re-anchoring; rebase the returned records afterwards so every
            // CUA result uses the same desktop frame.
            crate::wayland::remember_observed_window_origins(&windows);
            if let Ok(Some(frame)) = crate::wayland::current_desktop_frame() {
                frame.rebase_windows(&mut windows);
            }
        }
        let mut lines = vec![format!("Found {} windows:", windows.len())];
        for w in &windows {
            lines.push(window_list_line(w));
        }
        // Only in the text: a structured field would change the cross-platform
        // output record and its generated bindings.
        if let (true, Some(note)) = (
            none_listed,
            filter_pid.and_then(crate::atspi::unlisted_window_note),
        ) {
            lines.push(note);
        }
        let structured =
            json!({ "windows": windows.iter().map(window_record_json).collect::<Vec<_>>() });
        ToolResult::text(lines.join("\n")).with_structured(structured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(xid: u64, pid: u32, app: &str, on_screen: bool, z: Option<usize>) -> WindowInfo {
        WindowInfo {
            xid,
            pid: Some(pid),
            app_name: app.into(),
            title: String::new(),
            is_on_screen: on_screen,
            z_index: z,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }

    fn listed(
        mut windows: Vec<WindowInfo>,
        on_screen_only: bool,
        app: Option<&str>,
    ) -> Vec<(u64, Option<usize>)> {
        // pid 99 has exited.
        retain_listed(&mut windows, on_screen_only, app, |pid| pid != 99);
        windows.iter().map(|w| (w.xid, w.z_index)).collect()
    }

    #[test]
    fn filtered_z_indices_are_dense_and_keep_stacking_order() {
        // Raw stacking values as a backend reports them, front window last.
        let windows = vec![
            window(1, 10, "Firefox", true, Some(0)),
            window(2, 99, "Firefox", true, Some(1)),
            window(3, 10, "Firefox", false, Some(4)),
            window(4, 11, "gedit", true, Some(5)),
            window(5, 10, "Firefox", true, None),
            window(6, 10, "Firefox", true, Some(9)),
        ];
        assert_eq!(
            listed(windows.clone(), true, Some("firefox")),
            vec![(1, Some(0)), (5, None), (6, Some(1))]
        );
        assert_eq!(
            listed(windows, false, None),
            vec![
                (1, Some(0)),
                (3, Some(1)),
                (4, Some(2)),
                (5, None),
                (6, Some(3))
            ]
        );
    }

    #[test]
    fn visible_window_line_matches_the_upstream_shape() {
        let w = WindowInfo {
            xid: 7,
            pid: Some(10),
            app_name: "niri".into(),
            title: "Window".into(),
            is_on_screen: true,
            z_index: None,
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };
        assert_eq!(
            window_list_line(&w),
            "  window_id=7 pid=10 \"Window\" 300x200+10+20 app=niri"
        );
    }

    fn hidden_window_line_uses_a_hidden_bounds_token() {
        let line = window_list_line(&window(7, 10, "niri", false, None));
        assert!(line.contains(" hidden"), "{line}");
        assert!(!line.contains("0x0"), "{line}");
    }

    #[test]
    fn unordered_windows_stay_null() {
        let windows = vec![
            window(1, 10, "a", true, None),
            window(2, 10, "b", true, None),
        ];
        assert_eq!(listed(windows, false, None), vec![(1, None), (2, None)]);
    }
}
