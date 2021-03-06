/*
 * Supertag
 * Copyright (C) 2020 Andrew Moffat
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 */
use super::TAG;
use crate::common::notify::desktop::DesktopNotifier;
use crate::common::notify::uds::UDSNotifier;
use crate::common::settings::Settings;
use crate::common::types::cli::CliError;
use crate::sql::tpool::ThreadConnPool;
use crate::{common, fuse, sql};
use clap::ArgMatches;
use log::{debug, info};
use nix::unistd::{fork, ForkResult};
use parking_lot::Mutex;
use rusqlite::{Connection, Result as SqliteResult};
use std::error::Error;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

fn run_migrations<P: AsRef<Path>>(db_path: P) -> SqliteResult<()> {
    debug!(target: TAG, "Running migrations");
    let mut conn = Connection::open(&db_path)?;
    sql::migrations::migrate(&mut conn, &*common::version_str())?;
    Ok(())
}

pub fn handle(args: &ArgMatches, mut settings: Settings) -> Result<(), Box<dyn Error>> {
    info!(target: TAG, "Running mount");
    let col = args.value_of("collection").expect("Collection required!");
    settings.set_collection(col, true);

    let mountpoint = settings.mountpoint(col);
    println!("Mounting to {:?}", mountpoint);

    // only on linux do we have to mount over an existing directory
    // https://unix.stackexchange.com/questions/251090/why-does-mount-happen-over-an-existing-directory
    if cfg!(target_os = "linux") && !mountpoint.exists() {
        return Err(CliError::InvalidMountDir(mountpoint).into());
    }

    let db_path = settings.db_file(col);
    let share_settings = Arc::new(settings);

    let volicon = share_settings.volicon();
    let fuse_conf = fuse::util::make_fuse_config(volicon.as_deref());
    let mount_conf = fuse::util::make_mount_config(col, &db_path);

    let background = !args.is_present("foreground");

    if mountpoint.exists() {
        opener::open(&mountpoint)?;
    } else {
        opener::open(share_settings.supertag_dir())?;
    }

    if background {
        let conn_pool = ThreadConnPool::new(db_path.clone());
        debug!(target: TAG, "Forking into the background...");
        match fork().expect("Fork failed") {
            ForkResult::Parent { child } => {
                debug!(target: TAG, "Forked PID {}, now exiting", child);
                println!("Forked into background PID {}", child);
                Ok(())
            }
            ForkResult::Child => {
                // due to some weird bug on macos with rusqlite, we *cannot* do anything with the
                // database before we fork into the child, otherwise there is some kind of deadlock
                // with opening the database in the child process.
                //
                // i haven't been able to hunt down the cause of this yet, but it occurs even when
                // i am very careful to close + cleanup the database connection that existed in
                // the parent process. as such, we do the migrations here, to avoid the deadlock
                run_migrations(&db_path)?;

                debug!(target: TAG, "Creating notifier");
                let notifier = Arc::new(Mutex::new(DesktopNotifier::new(
                    share_settings.notification_icon(),
                )));

                debug!(target: TAG, "Creating TagFilesystem");
                let fsh = fuse::TagFilesystem::new(share_settings, conn_pool, notifier);
                debug!(target: TAG, "Mounting filesystem");
                let mount_handle = fuse_sys::mount(&mountpoint, fsh, false, fuse_conf, mount_conf)?;
                debug!(target: TAG, "Waiting on mount handle");
                mount_handle.lock().wait();
                debug!(target: TAG, "Done waiting on mount handle");
                Ok(())
            }
        }
    } else {
        run_migrations(&db_path)?;

        let conn_pool = ThreadConnPool::new(db_path.clone());
        info!(
            target: TAG,
            "Mounting {} to {}",
            db_path.display(),
            mountpoint.display()
        );

        let notifier_socket = share_settings.notify_socket_file(col);
        let notifier = Arc::new(Mutex::new(UDSNotifier::new(notifier_socket, true)?));

        let sigint = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::SIGINT, Arc::clone(&sigint))?;

        let fsh = fuse::TagFilesystem::new(share_settings, conn_pool, notifier);
        let _mount_handle = fuse_sys::mount(&mountpoint, fsh, false, fuse_conf, mount_conf)?;

        while !sigint.load(Ordering::Relaxed) {
            thread::sleep(std::time::Duration::from_millis(100));
        }
        info!(target: "mount", "Got SIGINT, unmounting and cleaning up");

        Ok(())
    }
}
