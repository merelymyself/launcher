mod wayland;
mod wayland_source;

use calloop::channel::*;
use wayland_client::Proxy;
use zbus::Connection;

use crate::*;
use freedesktop_desktop_entry as fde;
use futures::{
    channel::mpsc,
    future::{select, Either},
    StreamExt,
};
use pop_launcher::*;
use std::{ffi::OsString, fs, path::PathBuf};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use self::wayland::{spawn_toplevels, Toplevel, ToplevelAction, ToplevelEvent};

pub async fn main() {
    tracing::info!("starting cosmic-toplevel");

    let (mut app, mut toplevel_rx) = App::new(async_stdout(), Connection::session().await.unwrap());

    let mut requests = json_input_stream(async_stdin());
    let mut next_request = requests.next();
    let mut next_event = toplevel_rx.next();
    loop {
        let event = select(next_request, next_event).await;
        match event {
            Either::Left((Some(request), _next_event)) => {
                next_event = _next_event;
                next_request = requests.next();
                match request {
                    Ok(request) => match request {
                        Request::Activate(id) => {
                            tracing::info!("activating {id}");
                            app.activate(id).await
                        }
                        Request::Quit(id) => app.quit(id).await,
                        Request::Search(query) => {
                            tracing::info!("searching {query}");
                            app.search(&query).await;
                            // clear the ids to ignore, as all just sent are valid
                            app.ids_to_ignore.clear();
                        }
                        Request::Exit => break,
                        _ => (),
                    },
                    Err(why) => {
                        tracing::error!("malformed JSON request: {}", why);
                    }
                };
            }
            Either::Right((Some(event), _request)) => {
                next_event = toplevel_rx.next();
                next_request = _request;
                match event {
                    ToplevelEvent::Add(new_toplevel) => {
                        tracing::info!("{}", &new_toplevel.app_id);
                        app.toplevels
                            .retain(|t| t.toplevel_handle != new_toplevel.toplevel_handle);
                        app.toplevels.push(new_toplevel);
                    }
                    ToplevelEvent::Remove(remove_toplevel) => {
                        app.toplevels
                            .retain(|t| t.toplevel_handle != remove_toplevel.toplevel_handle);
                        // ignore requests for this id until after the next search
                        app.ids_to_ignore
                            .push(remove_toplevel.toplevel_handle.id().protocol_id());
                    }
                }
            }
            _ => break,
        }
    }
}

struct App<W> {
    desktop_entries: Vec<(fde::PathSource, PathBuf)>,
    ids_to_ignore: Vec<u32>,
    toplevels: Vec<Toplevel>,
    toplevel_tx: SyncSender<ToplevelAction>,
    tx: W,
    connection: Connection,
}

impl<W: AsyncWrite + Unpin> App<W> {
    fn new(tx: W, connection: Connection) -> (Self, mpsc::Receiver<ToplevelEvent>) {
        let (toplevels_tx, toplevel_rx) = mpsc::channel(100);

        (
            Self {
                ids_to_ignore: Default::default(),
                desktop_entries: fde::Iter::new(fde::default_paths())
                    .map(|path| (fde::PathSource::guess_from(&path), path))
                    .collect(),
                toplevels: Vec::new(),
                toplevel_tx: spawn_toplevels(toplevels_tx),
                tx,
                connection,
            },
            toplevel_rx,
        )
    }

    async fn activate(&mut self, id: u32) {
        tracing::info!("requested to activate: {id}");
        if self.ids_to_ignore.contains(&id) {
            return;
        }
        if let Some(handle) = self.toplevels.iter().find_map(|t| {
            if t.toplevel_handle.id().protocol_id() == id {
                Some(t.toplevel_handle.clone())
            } else {
                None
            }
        }) {
            tracing::info!("activating: {id}");
            let _ = self
                .connection
                .call_method(
                    Some("com.system76.CosmicAppletHost"),
                    "/com/system76/CosmicAppletHost",
                    Some("com.system76.CosmicAppletHost"),
                    "Toggle",
                    &("com.system76.CosmicLauncher"),
                )
                .await;
            let _ = self.toplevel_tx.send(ToplevelAction::Activate(handle));
        }
    }

    async fn quit(&mut self, id: u32) {
        if self.ids_to_ignore.contains(&id) {
            return;
        }
        if let Some(handle) = self.toplevels.iter().find_map(|t| {
            if t.toplevel_handle.id().protocol_id() == id {
                Some(t.toplevel_handle.clone())
            } else {
                None
            }
        }) {
            let _ = self.toplevel_tx.send(ToplevelAction::Close(handle));
        }
    }

    async fn search(&mut self, query: &str) {
        let query = query.to_ascii_lowercase();
        let haystack = query.split_ascii_whitespace().collect::<Vec<&str>>();

        fn contains_pattern(needle: &str, haystack: &[&str]) -> bool {
            let needle = needle.to_ascii_lowercase();
            haystack.iter().all(|h| needle.contains(h))
        }

        for item in self.toplevels.iter() {
            let retain = query.is_empty()
                || contains_pattern(&item.app_id, &haystack)
                || contains_pattern(&item.name, &haystack);

            if !retain {
                continue;
            }

            let mut icon_name = Cow::Borrowed("application-x-executable");

            for (_, path) in &self.desktop_entries {
                if let Some(name) = path.file_stem() {
                    let app_id: OsString = item.app_id.clone().into();
                    if app_id == name {
                        if let Ok(data) = fs::read_to_string(path) {
                            if let Ok(entry) = fde::DesktopEntry::decode(path, &data) {
                                if let Some(icon) = entry.icon() {
                                    icon_name = Cow::Owned(icon.to_owned());
                                }
                            }
                        }

                        break;
                    }
                }
            }

            send(
                &mut self.tx,
                PluginResponse::Append(PluginSearchResult {
                    // XXX protocol id may be re-used later
                    id: item.toplevel_handle.id().protocol_id(),
                    name: item.app_id.clone(),
                    description: item.name.clone(),
                    icon: Some(IconSource::Name(icon_name)),
                    ..Default::default()
                }),
            )
            .await;
        }

        send(&mut self.tx, PluginResponse::Finished).await;
        let _ = self.tx.flush();
    }
}
