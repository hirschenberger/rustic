use std::{collections::BTreeSet, process::Command};

use abscissa_core::Application;
use anyhow::Result;
use bytesize::ByteSize;
use comfy_table::{
    presets::ASCII_MARKDOWN, Attribute, Cell, CellAlignment, ContentArrangement, Table,
};

use log::{debug, info, trace, warn};
use rayon::{
    prelude::{IntoParallelRefIterator, ParallelBridge, ParallelIterator},
    ThreadPoolBuilder,
};

use rustic_core::{
    parse_command, BlobType, DecryptWriteBackend, FileType, Id, IndexBackend, IndexedBackend,
    Indexer, NodeType, OpenRepository, Packer, Progress, ProgressBars, ReadBackend, ReadIndex,
    RusticResult, SnapshotFile, TreeStreamerOnce, ALL_FILE_TYPES,
};

use crate::{application::RUSTIC_APP, config::progress_options::ProgressOptions};

pub(super) mod constants {
    pub(super) const MAX_READER_THREADS_NUM: usize = 20;
}

pub(crate) fn warm_up_wait(
    repo: &OpenRepository,
    packs: impl ExactSizeIterator<Item = Id>,
    wait: bool,
    progress_options: &ProgressOptions,
) -> Result<()> {
    if let Some(command) = &repo.opts.warm_up_command {
        warm_up_command(packs, command, progress_options)?;
    } else if repo.opts.warm_up {
        warm_up(&repo.be, packs, progress_options)?;
    }
    if wait {
        if let Some(wait) = repo.opts.warm_up_wait {
            let p = progress_options.progress_spinner(format!("waiting {wait}..."));
            std::thread::sleep(*wait);
            p.finish();
        }
    }
    Ok(())
}

pub(crate) fn warm_up_command(
    packs: impl ExactSizeIterator<Item = Id>,
    command: &str,
    progress_options: &ProgressOptions,
) -> Result<()> {
    let p = progress_options.progress_counter("warming up packs...");
    p.set_length(packs.len() as u64);
    for pack in packs {
        let actual_command = command.replace("%id", &pack.to_hex());
        debug!("calling {actual_command}...");
        let commands = parse_command::<()>(&actual_command)?.1;
        let status = Command::new(commands[0]).args(&commands[1..]).status()?;
        if !status.success() {
            warn!("warm-up command was not successful for pack {pack:?}. {status}");
        }
    }
    p.finish();
    Ok(())
}

pub(crate) fn warm_up(
    be: &impl ReadBackend,
    packs: impl ExactSizeIterator<Item = Id>,
    progress_options: &ProgressOptions,
) -> Result<()> {
    let mut be = be.clone();
    be.set_option("retry", "false")?;

    let p = progress_options.progress_counter("warming up packs...");
    p.set_length(packs.len() as u64);

    let pool = ThreadPoolBuilder::new()
        .num_threads(constants::MAX_READER_THREADS_NUM)
        .build()?;
    let p = &p;
    let be = &be;
    pool.in_place_scope(|s| {
        for pack in packs {
            s.spawn(move |_| {
                // ignore errors as they are expected from the warm-up
                _ = be.read_partial(FileType::Pack, &pack, false, 0, 1);
                p.inc(1);
            });
        }
    });

    p.finish();

    Ok(())
}

pub(crate) fn copy(
    snapshots: &[SnapshotFile],
    index: &impl IndexedBackend,
    repo_dest: &OpenRepository,
) -> Result<()> {
    let config = RUSTIC_APP.config();
    let be_dest = &repo_dest.dbe;
    let progress_options = &config.global.progress_options;

    let snapshots = relevant_snapshots(
        snapshots,
        repo_dest,
        |sn| config.snapshot_filter.matches(sn),
        &progress_options.progress_hidden(),
    )?;

    match (snapshots.len(), config.global.dry_run) {
        (count, true) => {
            info!("would have copied {count} snapshots");
            return Ok(());
        }
        (0, false) => {
            info!("no snapshot to copy.");
            return Ok(());
        }
        _ => {} // continue
    }

    let snap_trees: Vec<_> = snapshots.iter().map(|sn| sn.tree).collect();

    let index_dest = IndexBackend::new(be_dest, &progress_options.progress_counter(""))?;
    let indexer = Indexer::new(be_dest.clone()).into_shared();

    let data_packer = Packer::new(
        be_dest.clone(),
        BlobType::Data,
        indexer.clone(),
        &repo_dest.config,
        index.total_size(BlobType::Data),
    )?;
    let tree_packer = Packer::new(
        be_dest.clone(),
        BlobType::Tree,
        indexer.clone(),
        &repo_dest.config,
        index.total_size(BlobType::Tree),
    )?;

    let p = progress_options.progress_counter("copying blobs in snapshots...");

    snap_trees.par_iter().try_for_each(|id| -> Result<_> {
        trace!("copy tree blob {id}");
        if !index_dest.has_tree(id) {
            let data = index.get_tree(id).unwrap().read_data(index.be())?;
            tree_packer.add(data, *id)?;
        }
        Ok(())
    })?;

    let tree_streamer = TreeStreamerOnce::new(index.clone(), snap_trees, p)?;
    tree_streamer
        .par_bridge()
        .try_for_each(|item| -> Result<_> {
            let (_, tree) = item?;
            tree.nodes.par_iter().try_for_each(|node| {
                match node.node_type {
                    NodeType::File => {
                        node.content
                            .par_iter()
                            .flatten()
                            .try_for_each(|id| -> Result<_> {
                                trace!("copy data blob {id}");
                                if !index_dest.has_data(id) {
                                    let data = index.get_data(id).unwrap().read_data(index.be())?;
                                    data_packer.add(data, *id)?;
                                }
                                Ok(())
                            })?;
                    }

                    NodeType::Dir => {
                        let id = node.subtree.unwrap();
                        trace!("copy tree blob {id}");
                        if !index_dest.has_tree(&id) {
                            let data = index.get_tree(&id).unwrap().read_data(index.be())?;
                            tree_packer.add(data, id)?;
                        }
                    }

                    _ => {} // nothing to copy
                }
                Ok(())
            })
        })?;

    _ = data_packer.finalize()?;
    _ = tree_packer.finalize()?;
    indexer.write().unwrap().finalize()?;

    let p = progress_options.progress_counter("saving snapshots...");
    be_dest.save_list(snapshots.iter(), p)?;
    Ok(())
}

pub(crate) fn relevant_snapshots<F>(
    snaps: &[SnapshotFile],
    dest_repo: &OpenRepository,
    filter: F,
    p: &impl Progress,
) -> Result<Vec<SnapshotFile>>
where
    F: FnMut(&SnapshotFile) -> bool,
{
    // save snapshots in destination in BTreeSet, as we want to efficiently search within to filter out already existing snapshots before copying.
    let snapshots_dest: BTreeSet<_> = SnapshotFile::all_from_backend(&dest_repo.dbe, filter, p)?
        .into_iter()
        .map(SnapshotFile::clear_ids)
        .collect();
    let mut table = table_with_titles(["ID", "Time", "Host", "Label", "Tags", "Paths", "Status"]);
    let snaps = snaps
        .iter()
        .cloned()
        .map(|sn| (sn.id, SnapshotFile::clear_ids(sn)))
        .filter_map(|(id, sn)| {
            let relevant = !snapshots_dest.contains(&sn);
            let tags = sn.tags.formatln();
            let paths = sn.paths.formatln();
            let time = sn.time.format("%Y-%m-%d %H:%M:%S").to_string();
            _ = table.add_row([
                &id.to_string(),
                &time,
                &sn.hostname,
                &sn.label,
                &tags,
                &paths,
                &(if relevant { "to copy" } else { "existing" }).to_string(),
            ]);
            relevant.then_some(sn)
        })
        .collect();
    println!("{table}");

    Ok(snaps)
}

/// Helpers for table output

pub fn bold_cell<T: ToString>(s: T) -> Cell {
    Cell::new(s).add_attribute(Attribute::Bold)
}

#[must_use]
pub fn table() -> Table {
    let mut table = Table::new();
    _ = table
        .load_preset(ASCII_MARKDOWN)
        .set_content_arrangement(ContentArrangement::Dynamic);
    table
}

pub fn table_with_titles<I: IntoIterator<Item = T>, T: ToString>(titles: I) -> Table {
    let mut table = table();
    _ = table.set_header(titles.into_iter().map(bold_cell));
    table
}

pub fn table_right_from<I: IntoIterator<Item = T>, T: ToString>(start: usize, titles: I) -> Table {
    let mut table = table_with_titles(titles);
    // set alignment of all rows except first start row
    table
        .column_iter_mut()
        .skip(start)
        .for_each(|c| c.set_cell_alignment(CellAlignment::Right));

    table
}

pub fn print_file_info(text: &str, be: &impl ReadBackend) -> RusticResult<()> {
    info!("scanning files...");

    let mut table = table_right_from(1, ["File type", "Count", "Total Size"]);
    let mut total_count = 0;
    let mut total_size = 0;
    for tpe in ALL_FILE_TYPES {
        let list = be.list_with_size(tpe)?;
        let count = list.len();
        let size = list.iter().map(|f| u64::from(f.1)).sum();
        _ = table.add_row([
            format!("{tpe:?}"),
            count.to_string(),
            bytes_size_to_string(size),
        ]);
        total_count += count;
        total_size += size;
    }
    println!("{text}");
    _ = table.add_row([
        "Total".to_string(),
        total_count.to_string(),
        bytes_size_to_string(total_size),
    ]);

    println!();
    println!("{table}");
    println!();
    Ok(())
}

#[must_use]
pub fn bytes_size_to_string(b: u64) -> String {
    ByteSize(b).to_string_as(true)
}
