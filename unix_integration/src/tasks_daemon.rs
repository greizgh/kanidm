#![deny(warnings)]
#![warn(unused_extern_crates)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::unreachable)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::needless_pass_by_value)]
#![deny(clippy::trivially_copy_pass_by_ref)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;
use std::{fs, io};

use bytes::{BufMut, BytesMut};
use futures::{SinkExt, StreamExt};
use kanidm_unix_common::constants::DEFAULT_CONFIG_PATH;
use kanidm_unix_common::unix_config::KanidmUnixdConfig;
use kanidm_unix_common::unix_proto::{HomeDirectoryInfo, TaskRequest, TaskResponse};
use libc::{lchown, umask};
use sketching::tracing_forest::traits::*;
use sketching::tracing_forest::util::*;
use sketching::tracing_forest::{self};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tokio::time;
use tokio_util::codec::{Decoder, Encoder, Framed};
use users::{get_effective_gid, get_effective_uid};
use walkdir::WalkDir;

#[cfg(all(target_family = "unix", feature = "selinux"))]
use kanidm_unix_common::selinux_util;
#[cfg(all(target_family = "unix", feature = "selinux"))]
use selinux::SecurityContext;
#[cfg(all(target_family = "unix", feature = "selinux"))]
use std::process::Command;
#[cfg(all(target_family = "unix", feature = "selinux"))]
use users::get_user_by_uid;

struct TaskCodec;

impl Decoder for TaskCodec {
    type Error = io::Error;
    type Item = TaskRequest;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match serde_json::from_slice::<TaskRequest>(src) {
            Ok(msg) => {
                // Clear the buffer for the next message.
                src.clear();
                Ok(Some(msg))
            }
            _ => Ok(None),
        }
    }
}

impl Encoder<TaskResponse> for TaskCodec {
    type Error = io::Error;

    fn encode(&mut self, msg: TaskResponse, dst: &mut BytesMut) -> Result<(), Self::Error> {
        debug!("Attempting to send request -> {:?} ...", msg);
        let data = serde_json::to_vec(&msg).map_err(|e| {
            error!("socket encoding error -> {:?}", e);
            io::Error::new(io::ErrorKind::Other, "JSON encode error")
        })?;
        dst.put(data.as_slice());
        Ok(())
    }
}

impl TaskCodec {
    fn new() -> Self {
        TaskCodec
    }
}

fn chown(path: &Path, gid: u32) -> Result<(), String> {
    let path_os = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "Unable to create c-string".to_string())?;

    // Change the owner to the gid - remember, kanidm ONLY has gid's, the uid is implied.
    if unsafe { lchown(path_os.as_ptr(), gid, gid) } != 0 {
        return Err("Unable to set ownership".to_string());
    }
    Ok(())
}

fn create_home_directory(
    info: &HomeDirectoryInfo,
    home_prefix: &str,
    use_etc_skel: bool,
) -> Result<(), String> {
    // Final sanity check to prevent certain classes of attacks.
    let name = info.name.trim_start_matches('.').replace(['/', '\\'], "");

    let home_prefix_path = Path::new(home_prefix);

    // Does our home_prefix actually exist?
    if !home_prefix_path.exists() || !home_prefix_path.is_dir() {
        return Err("Invalid home_prefix from configuration".to_string());
    }

    // Actually process the request here.
    let hd_path_raw = format!("{}{}", home_prefix, name);
    let hd_path = Path::new(&hd_path_raw);

    // Assert the resulting named home path is consistent and correct.
    if let Some(pp) = hd_path.parent() {
        if pp != home_prefix_path {
            return Err("Invalid home directory name - not within home_prefix".to_string());
        }
    } else {
        return Err("Invalid/Corrupt home directory path - no prefix found".to_string());
    }

    // Get a handle to the SELinux labeling interface
    #[cfg(all(target_family = "unix", feature = "selinux"))]
    let labeler = selinux_util::get_labeler()?;

    // Construct a path for SELinux context lookups.
    // We do this because the policy only associates a home directory to its owning
    // user by the name of the directory. Since the real user's home directory is (by
    // default) their uuid or spn, its context will always be the policy default
    // (usually user_u or unconfined_u). This lookup path is used to ask the policy
    // what the context SHOULD be, and we will create policy equivalence rules below
    // so that relabels in the future do not break it.
    #[cfg(all(target_family = "unix", feature = "selinux"))]
    // Yes, gid, because we use the GID number for both the user's UID and primary GID
    let sel_lookup_path_raw = match get_user_by_uid(info.gid) {
        Some(v) => format!("{}{}", home_prefix, v.name().to_str().unwrap()),
        None => {
            return Err("Failed looking up username by uid for SELinux relabeling".to_string());
        }
    };

    // Does the home directory exist?
    if !hd_path.exists() {
        // Set a umask
        let before = unsafe { umask(0o0027) };

        // Set the SELinux security context for file creation
        #[cfg(all(target_family = "unix", feature = "selinux"))]
        selinux_util::do_setfscreatecon_for_path(&sel_lookup_path_raw, &labeler)?;

        // Create the dir
        if let Err(e) = fs::create_dir_all(hd_path) {
            let _ = unsafe { umask(before) };
            return Err(format!("{:?}", e));
        }
        let _ = unsafe { umask(before) };

        chown(hd_path, info.gid)?;

        // Copy in structure from /etc/skel/ if present
        let skel_dir = Path::new("/etc/skel/");
        if use_etc_skel && skel_dir.exists() {
            info!("preparing homedir using /etc/skel");
            for entry in WalkDir::new(skel_dir).into_iter().filter_map(|e| e.ok()) {
                let dest = &hd_path.join(
                    entry
                        .path()
                        .strip_prefix(skel_dir)
                        .map_err(|e| e.to_string())?,
                );

                #[cfg(all(target_family = "unix", feature = "selinux"))]
                {
                    // Look up the correct SELinux context of this object
                    let sel_lookup_path = Path::new(&sel_lookup_path_raw).join(
                        entry
                            .path()
                            .strip_prefix(skel_dir)
                            .map_err(|e| e.to_string())?,
                    );
                    selinux_util::do_setfscreatecon_for_path(
                        &sel_lookup_path.to_str().unwrap().to_string(),
                        &labeler,
                    )?;
                }

                if entry.path().is_dir() {
                    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
                } else {
                    fs::copy(entry.path(), dest).map_err(|e| e.to_string())?;
                }
                chown(dest, info.gid)?;

                // Create equivalence rule in the SELinux policy
                #[cfg(all(target_family = "unix", feature = "selinux"))]
                if Command::new("semanage")
                    .args(["fcontext", "-ae", &sel_lookup_path_raw, &hd_path_raw])
                    .spawn()
                    .is_err()
                {
                    return Err("Failed creating SELinux policy equivalence rule".to_string());
                }
            }
        }
    }

    // Reset object creation SELinux context to default
    #[cfg(all(target_family = "unix", feature = "selinux"))]
    if SecurityContext::set_default_context_for_new_file_system_objects().is_err() {
        return Err("Failed resetting SELinux file creation contexts".to_string());
    }

    let name_rel_path = Path::new(&name);
    // Does the aliases exist
    for alias in info.aliases.iter() {
        // Sanity check the alias.
        // let alias = alias.replace(".", "").replace("/", "").replace("\\", "");
        let alias = alias.trim_start_matches('.').replace(['/', '\\'], "");
        let alias_path_raw = format!("{}{}", home_prefix, alias);
        let alias_path = Path::new(&alias_path_raw);

        // Assert the resulting alias path is consistent and correct.
        if let Some(pp) = alias_path.parent() {
            if pp != home_prefix_path {
                return Err("Invalid home directory alias - not within home_prefix".to_string());
            }
        } else {
            return Err("Invalid/Corrupt alias directory path - no prefix found".to_string());
        }

        if alias_path.exists() {
            let attr = match fs::symlink_metadata(alias_path) {
                Ok(a) => a,
                Err(e) => {
                    return Err(format!("{:?}", e));
                }
            };

            if attr.file_type().is_symlink() {
                // Probably need to update it.
                if let Err(e) = fs::remove_file(alias_path) {
                    return Err(format!("{:?}", e));
                }
                if let Err(e) = symlink(name_rel_path, alias_path) {
                    return Err(format!("{:?}", e));
                }
            }
        } else {
            // Does not exist. Create.
            if let Err(e) = symlink(name_rel_path, alias_path) {
                return Err(format!("{:?}", e));
            }
        }
    }
    Ok(())
}

async fn handle_tasks(stream: UnixStream, cfg: &KanidmUnixdConfig) {
    let mut reqs = Framed::new(stream, TaskCodec::new());

    loop {
        match reqs.next().await {
            Some(Ok(TaskRequest::HomeDirectory(info))) => {
                debug!("Received task -> HomeDirectory({:?})", info);

                let resp = match create_home_directory(&info, &cfg.home_prefix, cfg.use_etc_skel) {
                    Ok(()) => TaskResponse::Success,
                    Err(msg) => TaskResponse::Error(msg),
                };

                // Now send a result.
                if let Err(e) = reqs.send(resp).await {
                    error!("Error -> {:?}", e);
                    return;
                }
                // All good, loop.
            }
            other => {
                error!("Error -> {:?}", other);
                return;
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // let cuid = get_current_uid();
    // let cgid = get_current_gid();
    // We only need to check effective id
    let ceuid = get_effective_uid();
    let cegid = get_effective_gid();

    tracing_forest::worker_task()
        .set_global(true)
        // Fall back to stderr
        .map_sender(|sender| sender.or_stderr())
        .build_on(|subscriber| {
            subscriber.with(
                EnvFilter::try_from_default_env()
                    .or_else(|_| EnvFilter::try_new("info"))
                    .expect("Failed to init envfilter"),
            )
        })
        .on(async {
            if ceuid != 0 || cegid != 0 {
                error!("Refusing to run - this process *MUST* operate as root.");
                return ExitCode::FAILURE;
            }

            let unixd_path = Path::new(DEFAULT_CONFIG_PATH);
            let unixd_path_str = match unixd_path.to_str() {
                Some(cps) => cps,
                None => {
                    error!("Unable to turn unixd_path to str");
                    return ExitCode::FAILURE;
                }
            };

            let cfg = match KanidmUnixdConfig::new().read_options_from_optional_config(unixd_path) {
                Ok(v) => v,
                Err(_) => {
                    error!("Failed to parse {}", unixd_path_str);
                    return ExitCode::FAILURE;
                }
            };

            let task_sock_path = cfg.task_sock_path.clone();
            debug!("Attempting to use {} ...", task_sock_path);

            let (broadcast_tx, mut broadcast_rx) = broadcast::channel(4);

            let server = tokio::spawn(async move {
                loop {
                    info!("Attempting to connect to kanidm_unixd ...");

                    tokio::select! {
                        _ = broadcast_rx.recv() => {
                            break;
                        }
                        connect_res = UnixStream::connect(&task_sock_path) => {
                            match connect_res {
                                Ok(stream) => {
                                    info!("Found kanidm_unixd, waiting for tasks ...");
                                    // Yep! Now let the main handler do it's job.
                                    // If it returns (dc, etc, then we loop and try again).
                                    handle_tasks(stream, &cfg).await;
                                }
                                Err(e) => {
                                    debug!("\\---> {:?}", e);
                                    error!("Unable to find kanidm_unixd, sleeping ...");
                                    // Back off.
                                    time::sleep(Duration::from_millis(5000)).await;
                                }
                            }
                        }
                    }
                }
            });

            info!("Server started ...");

            loop {
                tokio::select! {
                    Ok(()) = tokio::signal::ctrl_c() => {
                        break
                    }
                    Some(()) = async move {
                        let sigterm = tokio::signal::unix::SignalKind::terminate();
                        tokio::signal::unix::signal(sigterm).unwrap().recv().await
                    } => {
                        break
                    }
                    Some(()) = async move {
                        let sigterm = tokio::signal::unix::SignalKind::alarm();
                        tokio::signal::unix::signal(sigterm).unwrap().recv().await
                    } => {
                        // Ignore
                    }
                    Some(()) = async move {
                        let sigterm = tokio::signal::unix::SignalKind::hangup();
                        tokio::signal::unix::signal(sigterm).unwrap().recv().await
                    } => {
                        // Ignore
                    }
                    Some(()) = async move {
                        let sigterm = tokio::signal::unix::SignalKind::user_defined1();
                        tokio::signal::unix::signal(sigterm).unwrap().recv().await
                    } => {
                        // Ignore
                    }
                    Some(()) = async move {
                        let sigterm = tokio::signal::unix::SignalKind::user_defined2();
                        tokio::signal::unix::signal(sigterm).unwrap().recv().await
                    } => {
                        // Ignore
                    }
                }
            }
            info!("Signal received, shutting down");
            // Send a broadcast that we are done.
            if let Err(e) = broadcast_tx.send(true) {
                error!("Unable to shutdown workers {:?}", e);
            }

            let _ = server.await;
            ExitCode::SUCCESS
        })
        .await
}
