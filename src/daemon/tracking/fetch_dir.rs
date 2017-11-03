use std::sync::Arc;

use futures::{Future};
use tk_easyloop::spawn;

use disk::{self, Image};
use tracking::{Subsystem, Downloading};
use tracking::fetch_blocks::FetchBlocks;


pub fn start(sys: &Subsystem, cmd: Downloading) {
    let cmd = Arc::new(cmd);
    let cmd2 = cmd.clone();
    sys.rescan_dir(cmd.virtual_path.parent());
    sys.state().in_progress.insert(cmd.clone());
    sys.peers.notify_progress(
        &cmd.virtual_path, &cmd.image_id, cmd.mask.get(),
        sys.remote.has_image_source(&cmd.image_id));
    let sys = sys.clone();
    let sys2 = sys.clone();
    spawn(sys.images.get(&sys.tracking, &cmd.virtual_path, &cmd.image_id)
    .map_err(move |e| {
        error!("Error fetching index: {}. \
            We abort downloading of {} to {:?}", e,
            &cmd2.image_id, &cmd2.virtual_path);
        sys2.meta.dir_aborted(&cmd2.virtual_path);
        sys2.remote.notify_aborted_image(
            &cmd2.image_id, &cmd2.virtual_path,
            "cant_fetch_index".into());
    })
    .and_then(|index| {
        debug!("Got index {:?}", cmd.image_id);
        spawn(sys.disk.start_image(
                cmd.config.directory.clone(),
                index.clone(),
                cmd.virtual_path.clone(),
                cmd.replacing)
            .then(move |res| -> Result<(), ()> {
                match res {
                    Ok(img) => {
                        let img = Arc::new(img);
                        debug!("Created dir");
                        cmd.index_fetched(&img.index);
                        sys.peers.notify_progress(&cmd.virtual_path,
                            &cmd.image_id, cmd.mask.get(),
                            sys.remote.has_image_source(&cmd.image_id));
                        hardlink_blocks(sys.clone(), img, cmd);
                    }
                    Err(disk::Error::AlreadyExists) => {
                        sys.meta.dir_committed(&cmd.virtual_path);
                        sys.remote.notify_received_image(
                            &index.id, &cmd.virtual_path);
                        info!("Image already exists {:?}", cmd);
                    }
                    Err(e) => {
                        error!("Can't start image {:?}: {}",
                            cmd.virtual_path, e);
                        sys.meta.dir_aborted(&cmd.virtual_path);
                        sys.remote.notify_aborted_image(
                            &cmd.image_id, &cmd.virtual_path,
                            "cant_create_directory".into());
                    }
                }
                Ok(())
            }));
        Ok(())
    }));
}

fn hardlink_blocks(sys: Subsystem, image: Arc<Image>, cmd: Arc<Downloading>) {
    let sys2 = sys.clone();
    let cmd2 = cmd.clone();
    spawn(sys.meta.files_to_hardlink(&cmd.virtual_path, &image.index)
        .map(move |_sources| {
            //println!("Files {:#?}", sources);
            cmd.fill_blocks(&image.index);
            fetch_blocks(sys.clone(), image, cmd);
        })
        .map_err(move |e| {
            error!("Error fetching hardlink sources: {}", e);
            // TODO(tailhook) remove temporary directory
            sys2.meta.dir_aborted(&cmd2.virtual_path);
            sys2.remote.notify_aborted_image(
                &cmd2.image_id, &cmd2.virtual_path,
                "internal_error_when_hardlinking".into());
        }));
}

fn fetch_blocks(sys: Subsystem, image: Arc<Image>, cmd: Arc<Downloading>)
{
    let sys1 = sys.clone();
    let sys2 = sys.clone();
    let sys3 = sys.clone();
    let cmd1 = cmd.clone();
    let cmd3 = cmd.clone();
    spawn(FetchBlocks::new(&image, &cmd, &sys)
        .map_err(move |()| {
            // TODO(tailhook) remove temporary directory
            sys3.meta.dir_aborted(&cmd3.virtual_path);
            sys3.remote.notify_aborted_image(
                &cmd3.image_id, &cmd3.virtual_path,
                "cluster_abort_no_file_source".into());
        })
        .and_then(move |()| {
            sys1.disk.commit_image(image)
            .map_err(move |e| {
                error!("Error commiting image: {}", e);
                // TODO(tailhook) remove temporary directory
                sys1.meta.dir_aborted(&cmd1.virtual_path);
                sys1.remote.notify_aborted_image(
                    &cmd1.image_id, &cmd1.virtual_path,
                    "commit_error".into());
            })
        })
        .map(move |()| {
            sys2.meta.dir_committed(&cmd.virtual_path);
            sys2.remote.notify_received_image(
                &cmd.image_id, &cmd.virtual_path);
        }));
}
