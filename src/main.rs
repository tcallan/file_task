mod filesystem;
mod service;
mod terminal;

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{channel, Receiver},
    time::Duration,
};

use chrono::Local;
use clap::Parser;

use filesystem::{get_initial_state, update_file_items, FileGroup};
use service::{update_service_status, ServiceState};
use tui::{
    backend::Backend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame, Terminal,
};

use service::ServiceDetails;

const DELETED_RETENTION: Duration = Duration::from_secs(60 * 60 * 24); // one day
const INPUT_POLL: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    /// Paths to watch
    #[clap(required = true)]
    paths: Vec<PathBuf>,

    /// Systemd service to monitor
    #[clap(long)]
    service: Option<String>,
}

#[derive(Debug)]
struct AppState {
    file_groups: Vec<FileGroup>,
    service: Option<ServiceState>,
}

fn display_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|f| f.to_str())
        .or_else(|| path.to_str())
        .unwrap_or_default()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let app_state = AppState {
        file_groups: get_initial_state(args.paths)?,
        service: args.service.map(ServiceState::Unknown),
    };

    let (tx, rx) = channel();

    // NOTE: need to hold on to this so file watches continue to run
    let _watcher = filesystem::init_file_watch(tx, &app_state.file_groups)?;

    // setup terminal
    let mut state = terminal::TerminalState::init()?;

    run(&mut state.terminal, app_state, rx)?;

    Ok(())
}

fn update_state(rx: &Receiver<filesystem::FileChange>, state: &mut AppState) {
    update_file_items(rx, &mut state.file_groups);
    let status = update_service_status(state.service.as_ref());
    state.service = status;
}

fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    mut data: AppState,
    rx: Receiver<filesystem::FileChange>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        update_state(&rx, &mut data);
        terminal.draw(|f| ui(f, &data))?;

        if terminal::should_quit()? {
            return Ok(());
        }
    }
}

fn ui<B: Backend>(frame: &mut Frame<B>, state: &AppState) {
    const STATUS_BAR_HEIGHT: u16 = 1;
    let screen_area = frame.size();
    let file_group_count = state.file_groups.len() as u32;
    let file_group_space = screen_area.height - STATUS_BAR_HEIGHT;

    let layout_areas = Layout::default()
        .constraints([
            Constraint::Length(file_group_space),  // file list area
            Constraint::Length(STATUS_BAR_HEIGHT), // status bar
        ])
        .split(screen_area);

    let total_group_space = file_group_space as u32;
    let per_group_space = total_group_space / file_group_count;
    let extra_space = total_group_space % file_group_count;

    // divide the file list area up into even parts
    let constraints = (0..file_group_count)
        .map(|i| {
            Constraint::Ratio(
                // if there's any extra space give it to the first item
                per_group_space + (if i == 0 { extra_space } else { 0 }),
                total_group_space,
            )
        })
        .collect::<Vec<_>>();
    let file_list_areas = Layout::default()
        .constraints(constraints)
        .split(layout_areas[0]);

    for (group, rect) in state.file_groups.iter().zip(file_list_areas.iter()) {
        let list_items = group.items.iter().map(draw_file_item).collect::<Vec<_>>();
        let block = Block::default()
            .title(display_name(&group.root))
            .borders(Borders::ALL);
        let list = List::new(list_items).block(block).style(Style::default());
        frame.render_widget(list, *rect)
    }

    let time = draw_time();
    let service_status = draw_service_status(state);
    let content = Line::from(time.into_iter().chain(service_status).collect::<Vec<_>>());

    let bar = Paragraph::new(content).style(Style::default().bg(Color::Blue));

    frame.render_widget(bar, layout_areas[1]);
}

fn draw_time<'a>() -> Vec<Span<'a>> {
    let now = Local::now().format("%H:%M").to_string();
    let time = vec![
        Span::styled("[", Style::default().fg(Color::Cyan)),
        Span::styled(now, Style::default()),
        Span::styled("]", Style::default().fg(Color::Cyan)),
    ];
    time
}

fn draw_service_status(state: &AppState) -> Vec<Span> {
    if let Some(status) = &state.service {
        let (active, status_desc): (bool, &str) = match status {
            ServiceState::Details(ServiceDetails { active, status, .. }) => (*active, status),
            ServiceState::Unknown(_) => (false, "----"),
        };
        let status_style = if active {
            Style::default().fg(Color::Green)
        } else {
            Style::default().bg(Color::Red)
        };
        vec![
            Span::styled("[", Style::default().fg(Color::Cyan)),
            Span::styled(
                status_desc,
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .patch(status_style),
            ),
            Span::styled("]", Style::default().fg(Color::Cyan)),
        ]
    } else {
        vec![]
    }
}

fn draw_file_item(file: &filesystem::FileItem) -> ListItem {
    let color = if file.removed.is_none() {
        Color::Green
    } else {
        Color::LightBlue
    };
    ListItem::new(display_name(&file.path)).style(Style::default().fg(color))
}
