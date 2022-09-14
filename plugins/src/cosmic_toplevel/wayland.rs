use calloop::channel::{Event, SyncSender};
use cosmic_protocols::{
    toplevel_info::v1::client::{
        zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
        zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
    },
    toplevel_management::v1::client::zcosmic_toplevel_manager_v1::ZcosmicToplevelManagerV1,
    workspace::v1::client::{
        zcosmic_workspace_group_handle_v1::{self, ZcosmicWorkspaceGroupHandleV1},
        zcosmic_workspace_handle_v1::{self, ZcosmicWorkspaceHandleV1},
        zcosmic_workspace_manager_v1::{self, ZcosmicWorkspaceManagerV1},
    },
};
use futures::{channel::mpsc::Sender, SinkExt};
use std::{
    convert::{TryFrom, TryInto},
    env,
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{
    event_created_child,
    protocol::{
        wl_output::{self, WlOutput},
        wl_registry,
    },
    ConnectError, Proxy,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_source::WaylandSource;

use super::wayland_source;

#[derive(Debug, Clone)]
pub enum ToplevelAction {
    Activate(ZcosmicToplevelHandleV1),
    Close(ZcosmicToplevelHandleV1),
}

#[derive(Debug, Clone)]
pub enum ToplevelEvent {
    Add(Toplevel),
    Remove(Toplevel),
}

pub fn spawn_toplevels(tx: Sender<ToplevelEvent>) -> SyncSender<ToplevelAction> {
    let (workspaces_tx, workspaces_rx) = calloop::channel::sync_channel(100);

    if let Ok(Ok(conn)) = std::env::var("WAYLAND_DISPLAY")
        .map_err(anyhow::Error::msg)
        .map(|display_str| {
            let mut socket_path = env::var_os("XDG_RUNTIME_DIR")
                .map(Into::<PathBuf>::into)
                .ok_or(ConnectError::NoCompositor)?;
            socket_path.push(display_str);

            Ok(UnixStream::connect(socket_path).map_err(|_| ConnectError::NoCompositor)?)
        })
        .and_then(|s| s.map(|s| Connection::from_socket(s).map_err(anyhow::Error::msg)))
    {
        std::thread::spawn(move || {
            let mut event_loop = calloop::EventLoop::<State>::try_new().unwrap();
            let loop_handle = event_loop.handle();
            let event_queue = conn.new_event_queue::<State>();
            let qhandle = event_queue.handle();

            WaylandSource::new(event_queue)
                .expect("Failed to create wayland source")
                .insert(loop_handle)
                .unwrap();

            let display = conn.display();
            display.get_registry(&qhandle, ());

            let mut state = State {
                tx,
                workspace_manager: None,
                workspace_groups: Vec::new(),
                toplevel_info: None,
                toplevel_manager: None,
                running: true,
                toplevels: vec![],
                seats: vec![],
            };
            let loop_handle = event_loop.handle();
            loop_handle
                .insert_source(workspaces_rx, |e, _, state| match e {
                    Event::Msg(ToplevelAction::Activate(toplevel)) => {
                        if let Some(manager) = &state.toplevel_manager {
                            for seat in &state.seats {
                                manager.activate(&toplevel, seat)
                            }
                        }
                    }
                    Event::Msg(ToplevelAction::Close(t)) => {
                        if let Some(manager) = &state.toplevel_manager {
                            manager.close(&t);
                        }
                    }
                    Event::Closed => {
                        if let Some(workspace_manager) = &mut state.workspace_manager {
                            for g in &mut state.workspace_groups {
                                g.workspace_group_handle.destroy();
                            }
                            workspace_manager.stop();
                        }
                        if let Some(toplevel_manager) = &mut state.toplevel_manager {
                            toplevel_manager.destroy();
                        }
                        if let Some(toplevel_info) = &mut state.toplevel_info {
                            for toplevel in &state.toplevels {
                                toplevel.toplevel_handle.destroy();
                            }
                            toplevel_info.stop();
                        }
                    }
                })
                .unwrap();
            while state.running {
                event_loop
                    .dispatch(Duration::from_millis(16), &mut state)
                    .unwrap();
            }
        });
    } else {
        eprintln!("ENV variable WAYLAND_DISPLAY is missing. Exiting...");
        std::process::exit(1);
    }

    workspaces_tx
}

#[derive(Debug, Clone)]
pub struct State {
    tx: Sender<ToplevelEvent>,
    running: bool,
    workspace_manager: Option<ZcosmicWorkspaceManagerV1>,
    workspace_groups: Vec<WorkspaceGroup>,
    toplevel_info: Option<ZcosmicToplevelInfoV1>,
    toplevel_manager: Option<ZcosmicToplevelManagerV1>,
    toplevels: Vec<Toplevel>,
    seats: Vec<WlSeat>,
}

#[derive(Debug, Clone)]
pub struct Toplevel {
    pub name: String,
    pub app_id: String,
    pub toplevel_handle: ZcosmicToplevelHandleV1,
    pub states: Vec<zcosmic_toplevel_handle_v1::State>,
    pub output: Option<WlOutput>,
    pub workspace: Option<ZcosmicWorkspaceHandleV1>,
}

#[derive(Debug, Clone)]
struct WorkspaceGroup {
    workspace_group_handle: ZcosmicWorkspaceGroupHandleV1,
    output: Option<WlOutput>,
    workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone)]
struct Workspace {
    workspace_handle: ZcosmicWorkspaceHandleV1,
    name: String,
    coordinates: Vec<u32>,
    states: Vec<zcosmic_workspace_handle_v1::State>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match &interface[..] {
                "zcosmic_toplevel_info_v1" => {
                    let ti = registry.bind::<ZcosmicToplevelInfoV1, _, _>(name, 1, qh, ());
                    state.toplevel_info = Some(ti);
                }
                "zcosmic_toplevel_manager_v1" => {
                    let tm = registry.bind::<ZcosmicToplevelManagerV1, _, _>(name, 1, qh, ());
                    state.toplevel_manager = Some(tm);
                }
                "zcosmic_workspace_manager_v1" => {
                    let workspace_manager =
                        registry.bind::<ZcosmicWorkspaceManagerV1, _, _>(name, 1, qh, ());
                    state.workspace_manager = Some(workspace_manager);
                }
                "wl_seat" => {
                    registry.bind::<WlSeat, _, _>(name, 1, qh, ());
                }
                "wl_output" => {
                    registry.bind::<WlOutput, _, _>(name, 1, qh, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZcosmicToplevelInfoV1,
        event: <ZcosmicToplevelInfoV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zcosmic_toplevel_info_v1::Event::Toplevel { toplevel } => {
                state.toplevels.push(Toplevel {
                    name: "".into(),
                    app_id: "".into(),
                    toplevel_handle: toplevel,
                    states: vec![],
                    output: None,
                    workspace: None,
                });
            }
            zcosmic_toplevel_info_v1::Event::Finished => {
                todo!()
            }
            _ => {}
        }
    }

    event_created_child!(State, ZcosmicWorkspaceManagerV1, [
        0 => (ZcosmicToplevelHandleV1, ())
    ]);
}

impl Dispatch<ZcosmicToplevelManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZcosmicToplevelManagerV1,
        _event: <ZcosmicToplevelManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZcosmicToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        p: &ZcosmicToplevelHandleV1,
        event: <ZcosmicToplevelHandleV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zcosmic_toplevel_handle_v1::Event::Closed => {
                if let Some(i) = state.toplevels.iter().position(|t| &t.toplevel_handle == p) {
                    let removed_toplevel = state.toplevels.remove(i);
                    let _ = futures::executor::block_on(
                        state.tx.send(ToplevelEvent::Remove(removed_toplevel)),
                    );
                }
            }
            zcosmic_toplevel_handle_v1::Event::Done => {
                let to_send = state.toplevels.iter_mut().find(|t| &t.toplevel_handle == p);

                if let Some(toplevel) = to_send.cloned() {
                    let _ =
                        futures::executor::block_on(state.tx.send(ToplevelEvent::Add(toplevel)));
                }
            }
            zcosmic_toplevel_handle_v1::Event::Title { title } => {
                if let Some(i) = state.toplevels.iter_mut().find(|t| &t.toplevel_handle == p) {
                    i.name = title;
                }
            }
            zcosmic_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(i) = state.toplevels.iter_mut().find(|t| &t.toplevel_handle == p) {
                    i.app_id = app_id;
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputEnter { output } => {
                if let Some(i) = state.toplevels.iter_mut().find(|t| &t.toplevel_handle == p) {
                    i.output.replace(output);
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputLeave { output } => {
                if let Some(i) = state
                    .toplevels
                    .iter_mut()
                    .find(|t| &t.toplevel_handle == p && t.output.as_ref() == Some(&output))
                {
                    i.output.take();
                }
            }
            zcosmic_toplevel_handle_v1::Event::WorkspaceEnter { workspace } => {
                if let Some(i) = state.toplevels.iter_mut().find(|t| &t.toplevel_handle == p) {
                    i.workspace.replace(workspace);
                }
            }
            zcosmic_toplevel_handle_v1::Event::WorkspaceLeave { workspace } => {
                if let Some(i) = state
                    .toplevels
                    .iter_mut()
                    .find(|t| &t.toplevel_handle == p && t.workspace.as_ref() == Some(&workspace))
                {
                    i.workspace.take();
                }
            }
            zcosmic_toplevel_handle_v1::Event::State { state: t_state } => {
                if let Some(i) = state.toplevels.iter_mut().find(|t| &t.toplevel_handle == p) {
                    i.states = t_state
                        .chunks(4)
                        .map(|chunk| {
                            zcosmic_toplevel_handle_v1::State::try_from(u32::from_ne_bytes(
                                chunk.try_into().unwrap(),
                            ))
                            .unwrap()
                        })
                        .collect();
                }
            }
            _ => todo!(),
        }
    }
}

impl Dispatch<ZcosmicWorkspaceManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZcosmicWorkspaceManagerV1,
        event: zcosmic_workspace_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zcosmic_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                state.workspace_groups.push(WorkspaceGroup {
                    workspace_group_handle: workspace_group,
                    output: None,
                    workspaces: Vec::new(),
                });
            }
            zcosmic_workspace_manager_v1::Event::Done => {
                for group in &mut state.workspace_groups {
                    group.workspaces.sort_by(|w1, w2| {
                        w1.coordinates
                            .iter()
                            .zip(w2.coordinates.iter())
                            .find_map(|(coord1, coord2)| {
                                if coord1 != coord2 {
                                    Some(coord1.cmp(coord2))
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
            }
            zcosmic_workspace_manager_v1::Event::Finished => {
                state.workspace_manager.take();
            }
            _ => {}
        }
    }

    event_created_child!(State, ZcosmicWorkspaceManagerV1, [
        0 => (ZcosmicWorkspaceGroupHandleV1, ())
    ]);
}

impl Dispatch<ZcosmicWorkspaceGroupHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        group: &ZcosmicWorkspaceGroupHandleV1,
        event: zcosmic_workspace_group_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zcosmic_workspace_group_handle_v1::Event::OutputEnter { output } => {
                if let Some(group) = state
                    .workspace_groups
                    .iter_mut()
                    .find(|g| &g.workspace_group_handle == group)
                {
                    group.output = Some(output);
                }
            }
            zcosmic_workspace_group_handle_v1::Event::OutputLeave { output } => {
                if let Some(group) = state.workspace_groups.iter_mut().find(|g| {
                    &g.workspace_group_handle == group && g.output.as_ref() == Some(&output)
                }) {
                    group.output = None;
                }
            }
            zcosmic_workspace_group_handle_v1::Event::Workspace { workspace } => {
                if let Some(group) = state
                    .workspace_groups
                    .iter_mut()
                    .find(|g| &g.workspace_group_handle == group)
                {
                    group.workspaces.push(Workspace {
                        workspace_handle: workspace,
                        name: String::new(),
                        coordinates: Vec::new(),
                        states: Vec::new(),
                    })
                }
            }
            zcosmic_workspace_group_handle_v1::Event::Remove => {
                if let Some(group) = state
                    .workspace_groups
                    .iter()
                    .position(|g| &g.workspace_group_handle == group)
                {
                    state.workspace_groups.remove(group);
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, ZcosmicWorkspaceGroupHandleV1, [
        3 => (ZcosmicWorkspaceHandleV1, ())
    ]);
}

impl Dispatch<ZcosmicWorkspaceHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        workspace: &ZcosmicWorkspaceHandleV1,
        event: zcosmic_workspace_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zcosmic_workspace_handle_v1::Event::Name { name } => {
                if let Some(w) = state.workspace_groups.iter_mut().find_map(|g| {
                    g.workspaces
                        .iter_mut()
                        .find(|w| &w.workspace_handle == workspace)
                }) {
                    w.name = name;
                }
            }
            zcosmic_workspace_handle_v1::Event::Coordinates { coordinates } => {
                if let Some(w) = state.workspace_groups.iter_mut().find_map(|g| {
                    g.workspaces
                        .iter_mut()
                        .find(|w| &w.workspace_handle == workspace)
                }) {
                    // wayland is host byte order
                    w.coordinates = coordinates
                        .chunks(4)
                        .map(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap()))
                        .collect();
                }
            }
            zcosmic_workspace_handle_v1::Event::State {
                state: workspace_state,
            } => {
                if let Some(w) = state.workspace_groups.iter_mut().find_map(|g| {
                    g.workspaces
                        .iter_mut()
                        .find(|w| &w.workspace_handle == workspace)
                }) {
                    // wayland is host byte order
                    w.states = workspace_state
                        .chunks(4)
                        .map(|chunk| {
                            zcosmic_workspace_handle_v1::State::try_from(u32::from_ne_bytes(
                                chunk.try_into().unwrap(),
                            ))
                            .unwrap()
                        })
                        .collect();
                    // TODO if workspace active status changes while configured to only show active workspace, clear the list
                }
            }
            zcosmic_workspace_handle_v1::Event::Remove => {
                if let Some((g, w_i)) = state.workspace_groups.iter_mut().find_map(|g| {
                    g.workspaces
                        .iter_mut()
                        .position(|w| &w.workspace_handle == workspace)
                        .map(|p| (g, p))
                }) {
                    g.workspaces.remove(w_i);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        _state: &mut Self,
        _o: &WlOutput,
        _e: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.seats.iter().all(|s| s != seat) {
            state.seats.push(seat.clone());
        }
    }
}
