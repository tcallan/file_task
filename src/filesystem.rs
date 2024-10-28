use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};
use std::{fs, io};

use std::path::{Path, PathBuf};

use itertools::Itertools;
use notify::event::{ModifyKind, RenameMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebouncedEvent, Debouncer, NoCache};

use crate::DELETED_RETENTION;

#[derive(Debug, PartialEq, Eq)]
pub struct FileItem {
    pub path: PathBuf,
    pub removed: Option<Instant>,
}

impl FileItem {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            removed: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FileGroup {
    pub root: PathBuf,
    pub items: Vec<FileItem>,
}

#[cfg(test)]
impl FileGroup {
    fn new(root: PathBuf) -> FileGroup {
        FileGroup {
            root,
            items: vec![],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FileChange {
    Added(PathBuf),
    Removed(PathBuf),
    Moved(PathBuf, PathBuf),
}

pub fn get_initial_state(
    paths: Vec<PathBuf>,
) -> Result<Vec<FileGroup>, Box<dyn std::error::Error>> {
    for path in paths.iter() {
        if !path.exists() {
            return Err(format!("path {} does not exist", path.display()).into());
        }
        if !path.is_dir() {
            return Err(format!("path {} is not a directory", path.display()).into());
        }
    }

    paths
        .iter()
        .map(|path| read_initial_contents(path))
        .collect::<Result<Vec<_>, _>>()
}

fn read_initial_contents(path: &Path) -> Result<FileGroup, Box<dyn std::error::Error>> {
    let root = path.canonicalize()?;
    let contents = fs::read_dir(&root)?
        .map(|fr| fr.and_then(|f| f.path().canonicalize()).map(FileItem::new))
        .collect::<Result<Vec<_>, io::Error>>()?;

    Ok(FileGroup {
        root,
        items: contents,
    })
}

pub fn update_file_items(rx: &Receiver<FileChange>, file_items: &mut Vec<FileGroup>) {
    let now = Instant::now();
    // get any observed file changes
    let changes = rx.try_iter().collect::<Vec<_>>();

    // apply file changes
    for change in changes.iter() {
        match change {
            FileChange::Added(path) => {
                for group in find_groups(path, file_items) {
                    group.items.push(FileItem::new(path.to_path_buf()));
                }
            }
            FileChange::Removed(path) => {
                for group in find_groups(path, file_items) {
                    if let Some(existing) = group.items.iter_mut().find(|f| f.path == *path) {
                        existing.removed = Some(now);
                    }
                }
            }
            FileChange::Moved(from, to) => {
                if from.parent() == to.parent() {
                    // rename in same monitored group
                    for group in find_groups(from, file_items) {
                        if let Some(existing) = group.items.iter_mut().find(|f| f.path == *from) {
                            existing.path = to.to_path_buf();
                            // we might have already handled the "move from" part of this as a
                            // "remove", so fix up the removed state just in case
                            existing.removed = None;
                        }
                    }
                } else {
                    // was it moved to another tracked group?
                    let mut moved = false;

                    for group in find_groups(to, file_items) {
                        moved = true;
                        group.items.push(FileItem::new(to.to_path_buf()));
                    }

                    // if it was moved to another tracked group immediately remove it from the old one
                    // by abusing the standard cleanup; otherwise (i.e. it was moved out of tracking
                    // entirely) treat it as a normal deletion
                    let removed = if moved { now - DELETED_RETENTION } else { now };

                    for group in find_groups(from, file_items) {
                        if let Some(existing) = group.items.iter_mut().find(|f| f.path == *from) {
                            existing.removed = Some(removed);
                        }
                    }
                }
            }
        }
    }

    // clean up any expired removed files
    for group in file_items {
        group.items.retain(|f| {
            f.removed
                .map_or(true, |removed| removed.elapsed() <= DELETED_RETENTION)
        });
    }
}

fn find_groups<'a>(
    path: &'a Path,
    file_items: &'a mut [FileGroup],
) -> impl Iterator<Item = &'a mut FileGroup> + 'a {
    file_items
        .iter_mut()
        .filter(|f| path.starts_with(f.root.as_path()))
}

pub fn init_file_watch(
    tx: Sender<FileChange>,
    paths: &[FileGroup],
) -> Result<Debouncer<RecommendedWatcher, NoCache>, Box<dyn std::error::Error>> {
    let mut debouncer = new_debouncer(Duration::from_secs(2), None, move |res| match res {
        Ok(events) => handle_events(&tx, events),
        Err(e) => println!("watch error: {:?}", e),
    })?;

    for path in paths.iter() {
        debouncer
            .watch(&path.root, RecursiveMode::NonRecursive)?;
    }

    Ok(debouncer)
}

fn handle_events(tx: &Sender<FileChange>, events: Vec<DebouncedEvent>) {
    for dbe in events {
        handle_event(tx, dbe.event);
    }
}

fn handle_event(tx: &Sender<FileChange>, event: notify::Event) {
    // println!("{:?}", event);
    match event.kind {
        EventKind::Create(_) => event
            .paths
            .first()
            .map(|f| tx.send(FileChange::Added(f.to_path_buf()))),
        EventKind::Remove(_) => event
            .paths
            .first()
            .map(|f| tx.send(FileChange::Removed(f.to_owned()))),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => event
            .paths
            .iter()
            .next_tuple()
            .map(|(from, to)| tx.send(FileChange::Moved(from.to_owned(), to.to_owned()))),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => event
            .paths
            .first()
            // RenameMode::From means moved out of tracking; treat as a delete
            .map(|f| tx.send(FileChange::Removed(f.to_owned()))),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => event
            .paths
            .first()
            // RenameMode::To means moved in to tracking; treat as a create
            .map(|f| tx.send(FileChange::Added(f.to_owned()))),
        _ => None,
    };
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use itertools::assert_equal;

    use super::*;

    #[test]
    fn update_file_items_new() {
        let (tx, rx) = channel();
        let mut paths = vec![FileGroup::new(PathBuf::from("/root"))];

        tx.send(FileChange::Added(PathBuf::from("/root/foo")))
            .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 1);
        assert_equal(
            &paths[0].items,
            &vec![FileItem::new(PathBuf::from("/root/foo"))],
        );
    }

    #[test]
    fn update_file_items_new_duplicate() {
        let (tx, rx) = channel();
        let mut paths = vec![
            FileGroup::new(PathBuf::from("/root")),
            FileGroup::new(PathBuf::from("/root")),
        ];

        tx.send(FileChange::Added(PathBuf::from("/root/foo")))
            .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 2);
        let expected_items = vec![FileItem::new(PathBuf::from("/root/foo"))];
        assert_equal(&paths[0].items, &expected_items);
        assert_equal(&paths[1].items, &expected_items);
    }

    #[test]
    fn update_file_items_add() {
        let (tx, rx) = channel();
        let mut paths = vec![FileGroup {
            root: PathBuf::from("/root"),
            items: vec![FileItem::new(PathBuf::from("/root/bar"))],
        }];

        tx.send(FileChange::Added(PathBuf::from("/root/foo")))
            .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 1);
        let expected_items = vec![
            FileItem::new(PathBuf::from("/root/bar")),
            FileItem::new(PathBuf::from("/root/foo")),
        ];
        assert_equal(&paths[0].items, &expected_items);
    }

    #[test]
    fn update_file_items_add_duplicate() {
        let (tx, rx) = channel();
        let mut paths = vec![
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![FileItem::new(PathBuf::from("/root/bar"))],
            },
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![FileItem::new(PathBuf::from("/root/bar"))],
            },
        ];

        tx.send(FileChange::Added(PathBuf::from("/root/foo")))
            .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 2);
        let expected_items = vec![
            FileItem::new(PathBuf::from("/root/bar")),
            FileItem::new(PathBuf::from("/root/foo")),
        ];
        assert_equal(&paths[0].items, &expected_items);
        assert_equal(&paths[1].items, &expected_items);
    }

    #[test]
    fn update_file_items_delete_first() {
        let (tx, rx) = channel();
        let mut paths = vec![FileGroup {
            root: PathBuf::from("/root"),
            items: vec![
                FileItem::new(PathBuf::from("/root/bar")),
                FileItem::new(PathBuf::from("/root/foo")),
            ],
        }];

        tx.send(FileChange::Removed(PathBuf::from("/root/bar")))
            .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 1);
        let items = &paths[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].path, PathBuf::from("/root/bar"));
        assert!(items[0].removed.is_some());
        assert_eq!(items[1].path, PathBuf::from("/root/foo"));
        assert!(items[1].removed.is_none());
    }

    #[test]
    fn update_file_items_delete_second() {
        let (tx, rx) = channel();
        let mut paths = vec![FileGroup {
            root: PathBuf::from("/root"),
            items: vec![
                FileItem::new(PathBuf::from("/root/bar")),
                FileItem::new(PathBuf::from("/root/foo")),
            ],
        }];

        tx.send(FileChange::Removed(PathBuf::from("/root/foo")))
            .unwrap();

        update_file_items(&rx, &mut paths);
        assert_eq!(paths.len(), 1);
        let items = &paths[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].path, PathBuf::from("/root/bar"));
        assert!(items[0].removed.is_none());
        assert_eq!(items[1].path, PathBuf::from("/root/foo"));
        assert!(items[1].removed.is_some());
    }

    #[test]
    fn update_file_items_delete_first_duplicate() {
        let (tx, rx) = channel();
        let mut paths = vec![
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/foo")),
                ],
            },
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/foo")),
                ],
            },
        ];

        tx.send(FileChange::Removed(PathBuf::from("/root/bar")))
            .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 2);
        let assert_items = |items: &Vec<FileItem>| {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].path, PathBuf::from("/root/bar"));
            assert!(items[0].removed.is_some());
            assert_eq!(items[1].path, PathBuf::from("/root/foo"));
            assert!(items[1].removed.is_none());
        };
        assert_items(&paths[0].items);
        assert_items(&paths[1].items);
    }

    #[test]
    fn update_file_items_delete_second_duplicate() {
        let (tx, rx) = channel();
        let mut paths = vec![
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/foo")),
                ],
            },
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/foo")),
                ],
            },
        ];

        tx.send(FileChange::Removed(PathBuf::from("/root/foo")))
            .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 2);
        let assert_items = |items: &Vec<FileItem>| {
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].path, PathBuf::from("/root/bar"));
            assert!(items[0].removed.is_none());
            assert_eq!(items[1].path, PathBuf::from("/root/foo"));
            assert!(items[1].removed.is_some());
        };
        assert_items(&paths[0].items);
        assert_items(&paths[1].items);
    }

    #[test]
    fn update_file_items_move_first() {
        let (tx, rx) = channel();
        let mut paths = vec![FileGroup {
            root: PathBuf::from("/root"),
            items: vec![
                FileItem::new(PathBuf::from("/root/bar")),
                FileItem::new(PathBuf::from("/root/foo")),
            ],
        }];

        tx.send(FileChange::Moved(
            PathBuf::from("/root/bar"),
            PathBuf::from("/root/new"),
        ))
        .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 1);
        let expected_items = vec![
            FileItem::new(PathBuf::from("/root/new")),
            FileItem::new(PathBuf::from("/root/foo")),
        ];
        assert_equal(&paths[0].items, &expected_items);
    }

    #[test]
    fn update_file_items_move_second() {
        let (tx, rx) = channel();
        let mut paths = vec![FileGroup {
            root: PathBuf::from("/root"),
            items: vec![
                FileItem::new(PathBuf::from("/root/bar")),
                FileItem::new(PathBuf::from("/root/foo")),
            ],
        }];

        tx.send(FileChange::Moved(
            PathBuf::from("/root/foo"),
            PathBuf::from("/root/new"),
        ))
        .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 1);
        let expected_items = vec![
            FileItem::new(PathBuf::from("/root/bar")),
            FileItem::new(PathBuf::from("/root/new")),
        ];
        assert_equal(&paths[0].items, &expected_items);
    }

    #[test]
    fn update_file_items_move_first_duplicate() {
        let (tx, rx) = channel();
        let mut paths = vec![
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/foo")),
                ],
            },
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/foo")),
                ],
            },
        ];

        tx.send(FileChange::Moved(
            PathBuf::from("/root/bar"),
            PathBuf::from("/root/new"),
        ))
        .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 2);
        let expected_items = vec![
            FileItem::new(PathBuf::from("/root/new")),
            FileItem::new(PathBuf::from("/root/foo")),
        ];
        assert_equal(&paths[0].items, &expected_items);
        assert_equal(&paths[1].items, &expected_items);
    }

    #[test]
    fn update_file_items_move_second_duplicate() {
        let (tx, rx) = channel();
        let mut paths = vec![
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/foo")),
                ],
            },
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/foo")),
                ],
            },
        ];

        tx.send(FileChange::Moved(
            PathBuf::from("/root/foo"),
            PathBuf::from("/root/new"),
        ))
        .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 2);
        let expected_items = vec![
            FileItem::new(PathBuf::from("/root/bar")),
            FileItem::new(PathBuf::from("/root/new")),
        ];
        assert_equal(&paths[0].items, &expected_items);
        assert_equal(&paths[1].items, &expected_items);
    }

    #[test]
    fn update_file_items_move_out() {
        let (tx, rx) = channel();
        let mut paths = vec![FileGroup {
            root: PathBuf::from("/root"),
            items: vec![
                FileItem::new(PathBuf::from("/root/bar")),
                FileItem::new(PathBuf::from("/root/foo")),
            ],
        }];

        tx.send(FileChange::Moved(
            PathBuf::from("/root/bar"),
            PathBuf::from("/other/new"),
        ))
        .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 1);
        let items = &paths[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].path, PathBuf::from("/root/bar"));
        assert!(items[0].removed.is_some());
        assert_eq!(items[1].path, PathBuf::from("/root/foo"));
        assert!(items[1].removed.is_none());
    }

    #[test]
    fn update_file_items_move_in() {
        let (tx, rx) = channel();
        let mut paths = vec![FileGroup {
            root: PathBuf::from("/root"),
            items: vec![FileItem::new(PathBuf::from("/root/bar"))],
        }];

        tx.send(FileChange::Moved(
            PathBuf::from("/other/new"),
            PathBuf::from("/root/foo"),
        ))
        .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 1);
        let expected_items = vec![
            FileItem::new(PathBuf::from("/root/bar")),
            FileItem::new(PathBuf::from("/root/foo")),
        ];
        assert_eq!(&paths[0].items, &expected_items);
    }

    #[test]
    fn update_file_items_move_between() {
        let (tx, rx) = channel();
        let mut paths = vec![
            FileGroup {
                root: PathBuf::from("/root"),
                items: vec![
                    FileItem::new(PathBuf::from("/root/bar")),
                    FileItem::new(PathBuf::from("/root/move")),
                ],
            },
            FileGroup {
                root: PathBuf::from("/other"),
                items: vec![FileItem::new(PathBuf::from("/other/foo"))],
            },
        ];

        tx.send(FileChange::Moved(
            PathBuf::from("/root/move"),
            PathBuf::from("/other/move"),
        ))
        .unwrap();

        update_file_items(&rx, &mut paths);

        assert_eq!(paths.len(), 2);

        assert_eq!(paths[0].items.len(), 1);
        let items = &paths[0].items;
        assert_eq!(items[0].path, PathBuf::from("/root/bar"));
        assert!(items[0].removed.is_none());

        let expected_items_2 = vec![
            FileItem::new(PathBuf::from("/other/foo")),
            FileItem::new(PathBuf::from("/other/move")),
        ];
        assert_eq!(&paths[1].items, &expected_items_2);
    }
}
