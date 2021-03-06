use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;
use std::{fs, io};

use std::path::{Path, PathBuf};

use itertools::Itertools;
use notify::event::{ModifyKind, RenameMode};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{DELETED_RETENTION, WATCH_DEBOUNCE};

#[derive(Debug, PartialEq)]
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

#[derive(Debug, PartialEq)]
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

#[derive(Debug, PartialEq)]
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
                for group in find_groups(from, file_items) {
                    if let Some(existing) = group.items.iter_mut().find(|f| f.path == *from) {
                        existing.path = to.to_path_buf();
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
    file_items: &'a mut Vec<FileGroup>,
) -> impl Iterator<Item = &'a mut FileGroup> + 'a {
    file_items
        .iter_mut()
        .filter(|f| path.starts_with(f.root.as_path()))
}

pub fn init_file_watch(
    tx: Sender<FileChange>,
    paths: &[FileGroup],
) -> Result<RecommendedWatcher, Box<dyn std::error::Error>> {
    let mut watcher = notify::recommended_watcher(move |res| match res {
        Ok(event) => handle_event(&tx, event),
        Err(e) => println!("watch error: {:?}", e),
    })?;
    watcher.configure(Config::OngoingEvents(Some(WATCH_DEBOUNCE)))?;

    for path in paths.iter() {
        watcher.watch(&path.root, RecursiveMode::NonRecursive)?;
    }

    Ok(watcher)
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
}
