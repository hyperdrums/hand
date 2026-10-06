#![windows_subsystem = "windows"]

#[cfg(feature = "log-ui")]
use std::collections::VecDeque;

use std::os::windows::process::CommandExt;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use arboard::Clipboard;
#[cfg(feature = "log-ui")]
use eframe::egui::RichText;
use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, ScrollArea};
use serde::{Deserialize, Deserializer, Serialize};
use windows_sys::Win32::{
    System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    },
    UI::Shell::DragQueryFileW,
};

const MAX_HISTORY: usize = 100;
const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const CF_HDROP: u32 = 15;
const APP_VERSION: &str = "0.2.23";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_LAUNCHER_RECENTS: usize = 200;

#[derive(Default, Serialize, Deserialize)]
struct SavedState {
    favorites: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_icon_favorites")]
    icon_favorites: Vec<IconFavorite>,
    #[serde(default)]
    launcher_dirs: Vec<String>,
    #[serde(default)]
    launcher_recent: Vec<String>,
    #[serde(default)]
    launcher_sort: LauncherSort,
}

#[derive(Clone, Serialize, Deserialize)]
struct IconFavorite {
    path: String,
    label: String,
}

#[derive(Clone)]
struct LauncherFile {
    path: String,
    label: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegistrationKind {
    Path,
    Icon,
    LauncherDir,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchPane {
    Favorites,
    Launcher,
    Icons,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
enum LauncherSort {
    #[default]
    Name,
    Recent,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredIconFavorite {
    LegacyPath(String),
    Entry(IconFavorite),
}

fn deserialize_icon_favorites<'de, D>(deserializer: D) -> Result<Vec<IconFavorite>, D::Error>
where
    D: Deserializer<'de>,
{
    let entries = Vec::<StoredIconFavorite>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .map(|entry| match entry {
            StoredIconFavorite::LegacyPath(path) => IconFavorite {
                label: default_icon_label(&path),
                path,
            },
            StoredIconFavorite::Entry(entry) => entry,
        })
        .collect())
}

struct HandApp {
    history: Vec<String>,
    favorites: Vec<String>,
    icon_favorites: Vec<IconFavorite>,
    launcher_dirs: Vec<String>,
    launcher_files: Vec<LauncherFile>,
    launcher_recent: Vec<String>,
    launcher_sort: LauncherSort,
    favorite_input: String,
    icon_label_input: String,
    icon_edit_label_input: String,
    search_query: String,
    registration_kind: RegistrationKind,
    selected_history: Option<usize>,
    selected_favorite: Option<usize>,
    selected_launcher_file: Option<usize>,
    selected_icon_favorite: Option<usize>,
    selected_launcher_dir: Option<usize>,
    search_pane: SearchPane,
    search_result_highlight: bool,
    icon_textures: HashMap<String, Option<egui::TextureHandle>>,
    #[cfg(feature = "log-ui")]
    logs: VecDeque<(bool, String)>,
    #[cfg(feature = "log-ui")]
    log_collapsed: bool,
    last_clipboard_text: String,
    last_poll: Instant,
    config_path: PathBuf,
    save_blocked: bool,
}

impl HandApp {
    fn new() -> Self {
        let config_path = config_path();
        let mut app = Self {
            history: Vec::new(),
            favorites: Vec::new(),
            icon_favorites: Vec::new(),
            launcher_dirs: Vec::new(),
            launcher_files: Vec::new(),
            launcher_recent: Vec::new(),
            launcher_sort: LauncherSort::Name,
            favorite_input: String::new(),
            icon_label_input: String::new(),
            icon_edit_label_input: String::new(),
            search_query: String::new(),
            registration_kind: RegistrationKind::Path,
            selected_history: None,
            selected_favorite: None,
            selected_launcher_file: None,
            selected_icon_favorite: None,
            selected_launcher_dir: None,
            search_pane: SearchPane::Favorites,
            search_result_highlight: false,
            icon_textures: HashMap::new(),
            #[cfg(feature = "log-ui")]
            logs: VecDeque::new(),
            #[cfg(feature = "log-ui")]
            log_collapsed: false,
            last_clipboard_text: String::new(),
            last_poll: Instant::now(),
            config_path,
            save_blocked: false,
        };
        app.load_state();
        app.refresh_launcher_files();
        app
    }

    #[cfg(feature = "log-ui")]
    fn log(&mut self, message: impl Into<String>, is_error: bool) {
        if self.logs.len() == 200 {
            self.logs.pop_front();
        }
        self.logs.push_back((is_error, message.into()));
    }

    #[cfg(not(feature = "log-ui"))]
    fn log(&mut self, _message: impl Into<String>, _is_error: bool) {}

    fn matches_search(&self, value: &str) -> bool {
        let query = self.search_query.trim();
        query.is_empty() || value.to_lowercase().contains(&query.to_lowercase())
    }

    fn matching_favorite_indices(&self) -> Vec<usize> {
        self.favorites
            .iter()
            .enumerate()
            .filter_map(|(index, path)| self.matches_search(path).then_some(index))
            .collect()
    }

    fn matching_launcher_indices(&self) -> Vec<usize> {
        self.launcher_files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                (self.matches_search(&file.label) || self.matches_search(&file.path))
                    .then_some(index)
            })
            .collect()
    }

    fn matching_icon_indices(&self) -> Vec<usize> {
        self.icon_favorites
            .iter()
            .enumerate()
            .filter_map(|(index, favorite)| {
                (self.matches_search(&favorite.label) || self.matches_search(&favorite.path))
                    .then_some(index)
            })
            .collect()
    }

    fn select_search_result(&mut self, pane: SearchPane, index: Option<usize>) {
        self.search_pane = pane;
        self.search_result_highlight = !self.search_query.trim().is_empty();
        match pane {
            SearchPane::Favorites => {
                self.selected_favorite = index;
                self.selected_launcher_file = None;
                self.selected_icon_favorite = None;
            }
            SearchPane::Launcher => {
                self.selected_favorite = None;
                self.selected_launcher_file = index;
                self.selected_icon_favorite = None;
            }
            SearchPane::Icons => {
                self.selected_favorite = None;
                self.selected_launcher_file = None;
                if let Some(index) = index {
                    self.select_icon_favorite(index);
                } else {
                    self.selected_icon_favorite = None;
                    self.icon_edit_label_input.clear();
                }
            }
        }
    }

    fn move_search_selection(&mut self, step: isize) -> bool {
        let matches = match self.search_pane {
            SearchPane::Favorites => self.matching_favorite_indices(),
            SearchPane::Launcher => self.matching_launcher_indices(),
            SearchPane::Icons => self.matching_icon_indices(),
        };
        if matches.is_empty() {
            self.select_search_result(self.search_pane, None);
            return false;
        }

        let selected = match self.search_pane {
            SearchPane::Favorites => self.selected_favorite,
            SearchPane::Launcher => self.selected_launcher_file,
            SearchPane::Icons => self.selected_icon_favorite,
        };
        let next = selected
            .and_then(|index| matches.iter().position(|candidate| *candidate == index))
            .map(|position| {
                let len = matches.len() as isize;
                matches[(position as isize + step).rem_euclid(len) as usize]
            })
            .unwrap_or_else(|| {
                if step < 0 {
                    *matches.last().unwrap()
                } else {
                    matches[0]
                }
            });
        self.select_search_result(self.search_pane, Some(next));
        true
    }

    fn switch_search_pane(&mut self, pane: SearchPane) -> bool {
        let matches = match pane {
            SearchPane::Favorites => self.matching_favorite_indices(),
            SearchPane::Launcher => self.matching_launcher_indices(),
            SearchPane::Icons => self.matching_icon_indices(),
        };
        let selected = match pane {
            SearchPane::Favorites => self.selected_favorite,
            SearchPane::Launcher => self.selected_launcher_file,
            SearchPane::Icons => self.selected_icon_favorite,
        };
        let index = selected
            .filter(|index| matches.contains(index))
            .or_else(|| matches.first().copied());
        self.select_search_result(pane, index);
        index.is_some()
    }

    fn select_initial_search_result(&mut self) -> Option<SearchPane> {
        if let Some(index) = self.matching_favorite_indices().first().copied() {
            self.select_search_result(SearchPane::Favorites, Some(index));
            Some(SearchPane::Favorites)
        } else if let Some(index) = self.matching_launcher_indices().first().copied() {
            self.select_search_result(SearchPane::Launcher, Some(index));
            Some(SearchPane::Launcher)
        } else if let Some(index) = self.matching_icon_indices().first().copied() {
            self.select_search_result(SearchPane::Icons, Some(index));
            Some(SearchPane::Icons)
        } else {
            self.select_search_result(self.search_pane, None);
            None
        }
    }

    fn open_search_selection(&mut self) {
        let has_selection = match self.search_pane {
            SearchPane::Favorites => self
                .selected_favorite
                .is_some_and(|index| self.matching_favorite_indices().contains(&index)),
            SearchPane::Launcher => self
                .selected_launcher_file
                .is_some_and(|index| self.matching_launcher_indices().contains(&index)),
            SearchPane::Icons => self
                .selected_icon_favorite
                .is_some_and(|index| self.matching_icon_indices().contains(&index)),
        };
        if !has_selection && !self.switch_search_pane(self.search_pane) {
            return;
        }
        match self.search_pane {
            SearchPane::Favorites => self.open_selected_favorite(),
            SearchPane::Launcher => {
                if let Some(index) = self.selected_launcher_file {
                    self.open_launcher_file(index);
                }
            }
            SearchPane::Icons => self.open_selected_icon_favorite(),
        }
    }

    fn poll_clipboard(&mut self) {
        if self.last_poll.elapsed() < POLL_INTERVAL {
            return;
        }
        self.last_poll = Instant::now();

        let files = clipboard_files();
        if !files.is_empty() {
            self.last_clipboard_text.clear();
            self.add_history_items(files, "ファイルコピーを検出しました");
            return;
        }

        let Ok(mut clipboard) = Clipboard::new() else {
            return;
        };
        let Ok(text) = clipboard.get_text() else {
            return;
        };
        let text = text.trim().to_owned();
        if text.is_empty() {
            self.last_clipboard_text.clear();
        } else if text != self.last_clipboard_text {
            self.last_clipboard_text = text.clone();
            self.add_history_items(vec![text], "コピーを検出しました");
        }
    }

    fn add_history_items(&mut self, items: Vec<String>, log_message: &str) {
        let mut added = false;
        for item in items {
            if item.trim().is_empty() || self.history.contains(&item) {
                continue;
            }
            self.history.insert(0, item);
            added = true;
        }
        self.history.truncate(MAX_HISTORY);
        if added {
            self.selected_history = Some(0);
            self.log(log_message, false);
        }
    }

    fn register_favorite(&mut self) {
        let path = self.favorite_input.trim().to_owned();
        if path.is_empty() {
            return;
        }
        if !PathBuf::from(&path).exists() {
            self.log(format!("指定されたパスが見つかりません: {path}"), true);
            return;
        }
        if self.registration_kind == RegistrationKind::LauncherDir {
            if !Path::new(&path).is_dir() {
                self.log(
                    format!("ランチャーDIRにはフォルダを指定してください: {path}"),
                    true,
                );
                return;
            }
            if self.launcher_dirs.contains(&path) {
                self.log("このフォルダは既に登録されています。", true);
                return;
            }
            self.launcher_dirs.insert(0, path.clone());
            self.selected_launcher_dir = Some(0);
            self.refresh_launcher_files();
        } else if self.registration_kind == RegistrationKind::Icon {
            if self
                .icon_favorites
                .iter()
                .any(|favorite| favorite.path == path)
            {
                self.log("このパスは既に登録されています。", true);
                return;
            }
            let label = if self.icon_label_input.trim().is_empty() {
                default_icon_label(&path)
            } else {
                self.icon_label_input.trim().to_owned()
            };
            self.icon_favorites.insert(
                0,
                IconFavorite {
                    path: path.clone(),
                    label: label.clone(),
                },
            );
            self.selected_icon_favorite = Some(0);
            self.icon_edit_label_input = label;
        } else {
            if self.favorites.contains(&path) {
                self.log("このパスは既に登録されています。", true);
                return;
            }
            self.favorites.insert(0, path.clone());
            self.selected_favorite = Some(0);
        }
        self.favorite_input.clear();
        self.icon_label_input.clear();
        self.log(format!("お気に入りを追加しました: {path}"), false);
        self.save_state();
    }

    fn copy_selected_history(&mut self) {
        let Some(index) = self.selected_history else {
            return;
        };
        let Some(text) = self.history.get(index).cloned() else {
            return;
        };
        match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
            Ok(()) => self.log("履歴をコピーしました", false),
            Err(error) => self.log(format!("コピーに失敗しました: {error}"), true),
        }
    }

    fn remove_selected_history(&mut self) {
        let Some(index) = self.selected_history else {
            return;
        };
        if index >= self.history.len() {
            return;
        }
        self.history.remove(index);
        self.selected_history =
            (!self.history.is_empty()).then(|| index.min(self.history.len() - 1));
    }

    fn open_selected_history(&mut self) {
        let Some(index) = self.selected_history else {
            return;
        };
        let Some(value) = self.history.get(index).cloned() else {
            return;
        };
        if PathBuf::from(&value).exists()
            || ["http://", "https://", "file://"]
                .iter()
                .any(|prefix| value.starts_with(prefix))
        {
            match open_path(&value) {
                Ok(()) => self.log(format!("開きました: {value}"), false),
                Err(error) => self.log(format!("開けませんでした: {error}"), true),
            }
        } else {
            self.log(
                "開く対象がファイル/フォルダ/URLではありませんでした。",
                true,
            );
        }
    }

    fn open_selected_favorite(&mut self) {
        let Some(index) = self.selected_favorite else {
            return;
        };
        let Some(path) = self.favorites.get(index).cloned() else {
            return;
        };
        match open_path(&path) {
            Ok(()) => self.log(format!("開きました: {path}"), false),
            Err(error) => self.log(format!("開けませんでした: {error}"), true),
        }
    }

    fn remove_selected_favorite(&mut self) {
        let Some(index) = self.selected_favorite else {
            return;
        };
        if index >= self.favorites.len() {
            return;
        }
        let path = self.favorites.remove(index);
        self.selected_favorite =
            (!self.favorites.is_empty()).then(|| index.min(self.favorites.len() - 1));
        self.log(format!("お気に入りを削除しました: {path}"), false);
        self.save_state();
    }

    fn open_selected_icon_favorite(&mut self) {
        let Some(index) = self.selected_icon_favorite else {
            return;
        };
        let Some(favorite) = self.icon_favorites.get(index).cloned() else {
            return;
        };
        match open_path(&favorite.path) {
            Ok(()) => self.log(format!("開きました: {}", favorite.path), false),
            Err(error) => self.log(format!("開けませんでした: {error}"), true),
        }
    }

    fn select_icon_favorite(&mut self, index: usize) {
        let Some(favorite) = self.icon_favorites.get(index) else {
            return;
        };
        self.selected_icon_favorite = Some(index);
        self.icon_edit_label_input = favorite.label.clone();
    }

    fn save_icon_favorite_label(&mut self) {
        let Some(index) = self.selected_icon_favorite else {
            return;
        };
        let label = self.icon_edit_label_input.trim().to_owned();
        if label.is_empty() {
            return;
        }
        let Some(favorite) = self.icon_favorites.get_mut(index) else {
            return;
        };
        favorite.label = label;
        self.save_state();
    }

    fn remove_selected_icon_favorite(&mut self) {
        let Some(index) = self.selected_icon_favorite else {
            return;
        };
        if index >= self.icon_favorites.len() {
            return;
        }
        let favorite = self.icon_favorites.remove(index);
        self.icon_textures.remove(&favorite.path);
        self.selected_icon_favorite =
            (!self.icon_favorites.is_empty()).then(|| index.min(self.icon_favorites.len() - 1));
        self.log(format!("アイコンを削除しました: {}", favorite.path), false);
        self.save_state();
    }

    fn refresh_launcher_files(&mut self) {
        self.launcher_files.clear();
        self.selected_launcher_file = None;
        let Some(index) = self.selected_launcher_dir else {
            return;
        };
        let Some(directory) = self.launcher_dirs.get(index).cloned() else {
            return;
        };
        if fs::read_dir(&directory).is_err() {
            self.log(
                format!("ランチャーDIRを読み込めませんでした: {directory}"),
                true,
            );
            return;
        }

        collect_launcher_files(Path::new(&directory), &mut self.launcher_files);
        add_duplicate_parent_labels(&mut self.launcher_files);
        self.sort_launcher_files();
    }

    fn sort_launcher_files(&mut self) {
        let selected_path = self
            .selected_launcher_file
            .and_then(|index| self.launcher_files.get(index))
            .map(|file| file.path.clone());
        match self.launcher_sort {
            LauncherSort::Name => self
                .launcher_files
                .sort_by_cached_key(|file| file.label.to_lowercase()),
            LauncherSort::Recent => {
                let recent = self.launcher_recent.clone();
                self.launcher_files.sort_by_cached_key(|file| {
                    (
                        recent
                            .iter()
                            .position(|path| path == &file.path)
                            .unwrap_or(usize::MAX),
                        file.label.to_lowercase(),
                    )
                });
            }
        }
        self.selected_launcher_file = selected_path.and_then(|path| {
            self.launcher_files
                .iter()
                .position(|file| file.path == path)
        });
    }

    fn record_launcher_use(&mut self, path: &str) {
        self.launcher_recent
            .retain(|recent_path| recent_path != path);
        self.launcher_recent.insert(0, path.to_owned());
        self.launcher_recent.truncate(MAX_LAUNCHER_RECENTS);
        if self.launcher_sort == LauncherSort::Recent {
            self.sort_launcher_files();
        }
        self.save_state();
    }

    fn remove_selected_launcher_dir(&mut self) {
        let Some(index) = self.selected_launcher_dir else {
            return;
        };
        if index >= self.launcher_dirs.len() {
            return;
        }
        self.launcher_dirs.remove(index);
        self.selected_launcher_dir =
            (!self.launcher_dirs.is_empty()).then(|| index.min(self.launcher_dirs.len() - 1));
        self.refresh_launcher_files();
        self.save_state();
    }

    fn open_launcher_file(&mut self, index: usize) {
        let Some(file) = self.launcher_files.get(index) else {
            return;
        };
        let path = file.path.clone();
        let result = open_path(&path);
        if result.is_ok() {
            self.record_launcher_use(&path);
        }
        match result {
            Ok(()) => self.log(format!("開きました: {path}"), false),
            Err(error) => self.log(format!("開けませんでした: {error}"), true),
        }
    }

    fn cache_icon(&mut self, ctx: &egui::Context, path: &str) {
        if self.icon_textures.contains_key(path) {
            return;
        }
        let texture =
            windows_icons::get_icon_by_path_with_size(path, windows_icons::IconSize::Medium)
                .ok()
                .map(|image| {
                    let size = [image.width() as usize, image.height() as usize];
                    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
                    ctx.load_texture(
                        format!("file-icon:{path}"),
                        pixels,
                        egui::TextureOptions::LINEAR,
                    )
                });
        self.icon_textures.insert(path.to_owned(), texture);
    }

    fn load_state(&mut self) {
        match fs::read_to_string(&self.config_path) {
            Ok(contents) => match serde_json::from_str::<SavedState>(&contents) {
                Ok(state) => {
                    self.favorites = state
                        .favorites
                        .into_iter()
                        .filter(|value| !value.trim().is_empty())
                        .collect();
                    self.icon_favorites = state
                        .icon_favorites
                        .into_iter()
                        .filter(|favorite| !favorite.path.trim().is_empty())
                        .collect();
                    self.launcher_dirs = state
                        .launcher_dirs
                        .into_iter()
                        .filter(|path| Path::new(path).is_dir())
                        .collect();
                    self.launcher_recent = state.launcher_recent;
                    self.launcher_sort = state.launcher_sort;
                    self.selected_favorite = (!self.favorites.is_empty()).then_some(0);
                    self.selected_icon_favorite = (!self.icon_favorites.is_empty()).then_some(0);
                    self.selected_launcher_dir = (!self.launcher_dirs.is_empty()).then_some(0);
                    self.log(
                        format!("設定を読み込みました: {}", self.config_path.display()),
                        false,
                    );
                }
                Err(error) => {
                    self.log(format!("設定の読み込みに失敗しました: {error}"), true);
                    // 壊れた設定を空の状態で上書きしないよう、先に退避する。
                    let backup_path = self.config_path.with_extension("json.bak");
                    match fs::copy(&self.config_path, &backup_path) {
                        Ok(_) => self.log(
                            format!("壊れた設定を退避しました: {}", backup_path.display()),
                            true,
                        ),
                        Err(error) => {
                            self.save_blocked = true;
                            self.log(
                                format!("設定を退避できないため保存を停止します: {error}"),
                                true,
                            );
                        }
                    }
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.save_state(),
            Err(error) => {
                // 読めないだけで中身は無事な可能性があるため、上書きしない。
                self.save_blocked = true;
                self.log(format!("設定の読み込みに失敗しました: {error}"), true);
            }
        }
    }

    fn save_state(&mut self) {
        if self.save_blocked {
            self.log("設定の読み込みに失敗したため保存をスキップしました", true);
            return;
        }
        let state = SavedState {
            favorites: self.favorites.clone(),
            icon_favorites: self.icon_favorites.clone(),
            launcher_dirs: self.launcher_dirs.clone(),
            launcher_recent: self.launcher_recent.clone(),
            launcher_sort: self.launcher_sort,
        };
        match serde_json::to_string_pretty(&state)
            .and_then(|json| fs::write(&self.config_path, json).map_err(serde_json::Error::io))
        {
            Ok(()) => {}
            Err(error) => self.log(format!("設定の保存に失敗しました: {error}"), true),
        }
    }
}

impl eframe::App for HandApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_clipboard();
        ctx.request_repaint_after(POLL_INTERVAL);
        let search_id = egui::Id::new("global_search");
        let focus_search =
            ctx.input(|input| input.modifiers.ctrl && input.key_pressed(egui::Key::F));
        let search_has_focus = focus_search || ctx.memory(|memory| memory.has_focus(search_id));
        let mut scroll_to_favorite = false;
        let mut scroll_to_launcher = false;
        let mut scroll_to_icons = false;
        let mut search_changed = false;

        if search_has_focus {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                self.search_query.clear();
                self.selected_favorite = None;
                self.selected_launcher_file = None;
                self.selected_icon_favorite = None;
                self.search_result_highlight = false;
            } else if ctx
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
            {
                if self.move_search_selection(1) {
                    scroll_to_favorite = self.search_pane == SearchPane::Favorites;
                    scroll_to_launcher = self.search_pane == SearchPane::Launcher;
                    scroll_to_icons = self.search_pane == SearchPane::Icons;
                }
            } else if ctx
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp))
            {
                if self.move_search_selection(-1) {
                    scroll_to_favorite = self.search_pane == SearchPane::Favorites;
                    scroll_to_launcher = self.search_pane == SearchPane::Launcher;
                    scroll_to_icons = self.search_pane == SearchPane::Icons;
                }
            } else if ctx
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft))
            {
                let next_pane = match self.search_pane {
                    SearchPane::Favorites => SearchPane::Icons,
                    SearchPane::Launcher => SearchPane::Favorites,
                    SearchPane::Icons => SearchPane::Launcher,
                };
                let has_selection = self.switch_search_pane(next_pane);
                scroll_to_favorite = has_selection && next_pane == SearchPane::Favorites;
                scroll_to_launcher = has_selection && next_pane == SearchPane::Launcher;
                scroll_to_icons = has_selection && next_pane == SearchPane::Icons;
            } else if ctx
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight))
            {
                let next_pane = match self.search_pane {
                    SearchPane::Favorites => SearchPane::Launcher,
                    SearchPane::Launcher => SearchPane::Icons,
                    SearchPane::Icons => SearchPane::Favorites,
                };
                let has_selection = self.switch_search_pane(next_pane);
                scroll_to_favorite = has_selection && next_pane == SearchPane::Favorites;
                scroll_to_launcher = has_selection && next_pane == SearchPane::Launcher;
                scroll_to_icons = has_selection && next_pane == SearchPane::Icons;
            } else if ctx
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
            {
                self.open_search_selection();
            }
        }

        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 10)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let path_input_width = if self.registration_kind == RegistrationKind::Icon {
                        280.0
                    } else {
                        400.0
                    };
                    let response = ui.add_sized(
                        [path_input_width, 28.0],
                        egui::TextEdit::singleline(&mut self.favorite_input)
                            .hint_text("ファイルまたはフォルダのパス"),
                    );
                    egui::ComboBox::from_id_salt("favorite_register_mode")
                        .selected_text(match self.registration_kind {
                            RegistrationKind::Path => "パス",
                            RegistrationKind::Icon => "アイコン",
                            RegistrationKind::LauncherDir => "ディレクトリ",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.registration_kind,
                                RegistrationKind::Path,
                                "パス",
                            );
                            ui.selectable_value(
                                &mut self.registration_kind,
                                RegistrationKind::Icon,
                                "アイコン",
                            );
                            ui.selectable_value(
                                &mut self.registration_kind,
                                RegistrationKind::LauncherDir,
                                "ディレクトリ",
                            );
                        });
                    if self.registration_kind == RegistrationKind::Icon {
                        ui.add_sized(
                            [150.0, 28.0],
                            egui::TextEdit::singleline(&mut self.icon_label_input)
                                .hint_text("表示名"),
                        );
                    }
                    if response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        self.register_favorite();
                    }
                    if ui.button("追加").clicked() {
                        self.register_favorite();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let search_response = ui.add_sized(
                            [240.0, 28.0],
                            egui::TextEdit::singleline(&mut self.search_query)
                                .id(search_id)
                                .hint_text("検索(Ctrl+F)")
                                .return_key(None),
                        );
                        if focus_search {
                            search_response.request_focus();
                        }
                        search_changed = search_response.changed();
                    });
                });
            });

        if search_changed {
            if self.search_query.trim().is_empty() {
                self.search_result_highlight = false;
            } else {
                match self.select_initial_search_result() {
                    Some(SearchPane::Favorites) => scroll_to_favorite = true,
                    Some(SearchPane::Launcher) => scroll_to_launcher = true,
                    Some(SearchPane::Icons) => scroll_to_icons = true,
                    None => {}
                }
            }
        }

        #[cfg(feature = "log-ui")]
        egui::TopBottomPanel::bottom("log_panel")
            .resizable(!self.log_collapsed)
            .default_height(if self.log_collapsed { 38.0 } else { 150.0 })
            .min_height(38.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("ログ");
                    let label = if self.log_collapsed {
                        "展開"
                    } else {
                        "折りたたむ"
                    };
                    if ui.small_button(label).clicked() {
                        self.log_collapsed = !self.log_collapsed;
                    }
                });
                if !self.log_collapsed {
                    ui.separator();
                    ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                        for (is_error, message) in &self.logs {
                            let color = if *is_error {
                                Color32::RED
                            } else {
                                ui.visuals().text_color()
                            };
                            let prefix = if *is_error { "[ERROR]" } else { "[INFO ]" };
                            ui.label(RichText::new(format!("{prefix} {message}")).color(color));
                        }
                    });
                }
            });

        egui::SidePanel::left("history_panel")
            .resizable(true)
            .default_width(330.0)
            .min_width(230.0)
            .show(ctx, |ui| {
                ui.visuals_mut().override_text_color = Some(Color32::BLACK);
                ui.heading("履歴");
                ui.horizontal(|ui| {
                    if ui.button("開く").clicked() {
                        self.open_selected_history();
                    }
                });
                ui.separator();
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for index in 0..self.history.len() {
                            let item = &self.history[index];
                            if !self.matches_search(item) {
                                continue;
                            }
                            let response = ui.add(
                                egui::Button::new(item)
                                    .min_size(egui::vec2(ui.available_width(), 24.0))
                                    .selected(self.selected_history == Some(index)),
                            );
                            if response.clicked() {
                                self.selected_history = Some(index);
                            }
                            if response.double_clicked() {
                                self.selected_history = Some(index);
                                self.copy_selected_history();
                            }
                        }
                    });
                if ui.input(|input| input.key_pressed(egui::Key::Delete)) {
                    self.remove_selected_history();
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.visuals_mut().override_text_color = Some(Color32::BLACK);
            let panel_rect = ui.max_rect();
            let divider_color = Color32::from_gray(180);
            ui.columns(2, |columns| {
                let divider_x =
                    (columns[0].max_rect().right() + columns[1].max_rect().left()) / 2.0;
                columns[0].painter().line_segment(
                    [
                        egui::pos2(divider_x, panel_rect.top()),
                        egui::pos2(divider_x, panel_rect.bottom()),
                    ],
                    egui::Stroke::new(1.0_f32, divider_color),
                );
                let favorites_ui = &mut columns[0];
                favorites_ui.horizontal(|ui| {
                    if ui
                        .add_enabled(self.selected_favorite.is_some(), egui::Button::new("削除"))
                        .clicked()
                    {
                        self.remove_selected_favorite();
                    }
                });
                favorites_ui.separator();
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(favorites_ui, |ui| {
                        for index in 0..self.favorites.len() {
                            let path = &self.favorites[index];
                            if !self.matches_search(path) {
                                continue;
                            }
                            let is_search_highlight = self.search_result_highlight
                                && self.search_pane == SearchPane::Favorites
                                && self.selected_favorite == Some(index);
                            let button = egui::Button::new(path)
                                .min_size(egui::vec2(ui.available_width(), 24.0));
                            let button = if is_search_highlight {
                                button.fill(Color32::from_rgb(255, 196, 110)).stroke(
                                    egui::Stroke::new(1.0_f32, Color32::from_rgb(190, 105, 20)),
                                )
                            } else {
                                button.selected(self.selected_favorite == Some(index))
                            };
                            let response = ui.add(button);
                            if response.clicked() {
                                self.selected_favorite = Some(index);
                                self.selected_launcher_file = None;
                                self.selected_icon_favorite = None;
                                self.search_pane = SearchPane::Favorites;
                                self.search_result_highlight = false;
                            }
                            if response.double_clicked() {
                                self.selected_favorite = Some(index);
                                self.open_selected_favorite();
                            }
                            if scroll_to_favorite && self.selected_favorite == Some(index) {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                        }
                    });

                let icons_ui = &mut columns[1];
                let selected_dir_label = self
                    .selected_launcher_dir
                    .and_then(|index| self.launcher_dirs.get(index))
                    .and_then(|path| Path::new(path).file_name())
                    .and_then(|name| name.to_str())
                    .unwrap_or("フォルダ未登録")
                    .to_owned();
                let mut selected_dir_changed = false;
                icons_ui.horizontal(|ui| {
                    let sort_label = match self.launcher_sort {
                        LauncherSort::Name => "文字順",
                        LauncherSort::Recent => "最近使った",
                    };
                    if ui
                        .add(
                            egui::Button::new(sort_label)
                                .selected(self.launcher_sort == LauncherSort::Recent),
                        )
                        .clicked()
                    {
                        self.launcher_sort = match self.launcher_sort {
                            LauncherSort::Name => LauncherSort::Recent,
                            LauncherSort::Recent => LauncherSort::Name,
                        };
                        self.sort_launcher_files();
                        self.save_state();
                    }
                    egui::ComboBox::from_id_salt("launcher_directory")
                        .selected_text(selected_dir_label)
                        .show_ui(ui, |ui| {
                            for index in 0..self.launcher_dirs.len() {
                                let path = &self.launcher_dirs[index];
                                let label = Path::new(path)
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or(path);
                                if ui
                                    .selectable_value(
                                        &mut self.selected_launcher_dir,
                                        Some(index),
                                        label,
                                    )
                                    .clicked()
                                {
                                    selected_dir_changed = true;
                                }
                            }
                        });
                    if ui.small_button("更新").clicked() {
                        self.refresh_launcher_files();
                    }
                    if ui
                        .add_enabled(
                            self.selected_launcher_dir.is_some(),
                            egui::Button::new("削除"),
                        )
                        .clicked()
                    {
                        self.remove_selected_launcher_dir();
                    }
                });
                if selected_dir_changed {
                    self.refresh_launcher_files();
                }
                let launcher_height = (icons_ui.available_height() * 0.48).max(110.0);
                ScrollArea::vertical()
                    .max_height(launcher_height)
                    .show(icons_ui, |ui| {
                        for index in 0..self.launcher_files.len() {
                            let file = self.launcher_files[index].clone();
                            if !self.matches_search(&file.label) && !self.matches_search(&file.path)
                            {
                                continue;
                            }
                            self.cache_icon(ctx, &file.path);
                            let button = match self
                                .icon_textures
                                .get(&file.path)
                                .and_then(|icon| icon.as_ref())
                            {
                                Some(icon) => egui::Button::image_and_text(
                                    egui::Image::new((icon.id(), egui::vec2(20.0, 20.0))),
                                    &file.label,
                                ),
                                None => egui::Button::new(format!("?  {}", file.label)),
                            };
                            let is_search_highlight = self.search_result_highlight
                                && self.search_pane == SearchPane::Launcher
                                && self.selected_launcher_file == Some(index);
                            let button = if is_search_highlight {
                                button.fill(Color32::from_rgb(255, 196, 110)).stroke(
                                    egui::Stroke::new(1.0_f32, Color32::from_rgb(190, 105, 20)),
                                )
                            } else {
                                button.selected(self.selected_launcher_file == Some(index))
                            };
                            let response = ui
                                .add_sized([ui.available_width(), 24.0], button)
                                .on_hover_text(&file.path);
                            if response.clicked() {
                                self.selected_launcher_file = Some(index);
                                self.selected_favorite = None;
                                self.selected_icon_favorite = None;
                                self.search_pane = SearchPane::Launcher;
                                self.search_result_highlight = false;
                            }
                            if response.double_clicked() {
                                self.selected_launcher_file = Some(index);
                                self.open_launcher_file(index);
                            }
                            if scroll_to_launcher && self.selected_launcher_file == Some(index) {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                        }
                    });
                icons_ui.separator();
                icons_ui.horizontal(|ui| {
                    ui.add_enabled_ui(self.selected_icon_favorite.is_some(), |ui| {
                        ui.add_sized(
                            [150.0, 24.0],
                            egui::TextEdit::singleline(&mut self.icon_edit_label_input)
                                .hint_text("表示名"),
                        );
                        if ui.button("保存").clicked() {
                            self.save_icon_favorite_label();
                        }
                    });
                    if ui
                        .add_enabled(
                            self.selected_icon_favorite.is_some(),
                            egui::Button::new("削除"),
                        )
                        .clicked()
                    {
                        self.remove_selected_icon_favorite();
                    }
                });
                icons_ui.separator();
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(icons_ui, |ui| {
                        for index in 0..self.icon_favorites.len() {
                            let favorite = self.icon_favorites[index].clone();
                            if !self.matches_search(&favorite.label)
                                && !self.matches_search(&favorite.path)
                            {
                                continue;
                            }
                            self.cache_icon(ctx, &favorite.path);
                            let button = match self
                                .icon_textures
                                .get(&favorite.path)
                                .and_then(|icon| icon.as_ref())
                            {
                                Some(icon) => egui::Button::image_and_text(
                                    egui::Image::new((icon.id(), egui::vec2(20.0, 20.0))),
                                    &favorite.label,
                                ),
                                None => egui::Button::new(format!("?  {}", favorite.label)),
                            };
                            let is_search_highlight = self.search_result_highlight
                                && self.search_pane == SearchPane::Icons
                                && self.selected_icon_favorite == Some(index);
                            let button = if is_search_highlight {
                                button.fill(Color32::from_rgb(255, 196, 110)).stroke(
                                    egui::Stroke::new(1.0_f32, Color32::from_rgb(190, 105, 20)),
                                )
                            } else {
                                button.selected(self.selected_icon_favorite == Some(index))
                            };
                            let response = ui
                                .add_sized([ui.available_width(), 24.0], button)
                                .on_hover_text(&favorite.path);
                            if response.clicked() {
                                self.select_icon_favorite(index);
                                self.selected_favorite = None;
                                self.selected_launcher_file = None;
                                self.search_pane = SearchPane::Icons;
                                self.search_result_highlight = false;
                            }
                            if response.double_clicked() {
                                self.select_icon_favorite(index);
                                self.open_selected_icon_favorite();
                            }
                            if scroll_to_icons && self.selected_icon_favorite == Some(index) {
                                response.scroll_to_me(Some(egui::Align::Center));
                            }
                        }
                    });
            });
        });

        return;

        #[cfg(all(feature = "legacy-layout", feature = "log-ui"))]
        #[allow(unreachable_code)]
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.heading("クリップボード履歴");
            ui.horizontal(|ui| {
                if ui.button("開く").clicked() {
                    self.open_selected_history();
                }
                if ui.button("コピー").clicked() {
                    self.copy_selected_history();
                }
            });
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ScrollArea::vertical().max_height(185.0).show(ui, |ui| {
                    for index in 0..self.history.len() {
                        let item = &self.history[index];
                        let response =
                            ui.selectable_label(self.selected_history == Some(index), item);
                        if response.clicked() {
                            self.selected_history = Some(index);
                        }
                        if response.double_clicked() {
                            self.selected_history = Some(index);
                            self.copy_selected_history();
                        }
                    }
                });
            });

            ui.add_space(4.0);
            ui.heading("お気に入り");
            ui.horizontal(|ui| {
                ui.label("パス:");
                let response = ui.text_edit_singleline(&mut self.favorite_input);
                egui::ComboBox::from_id_salt("favorite_register_mode")
                    .selected_text(if self.registration_kind == RegistrationKind::Icon {
                        "アイコン"
                    } else {
                        "パス"
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.registration_kind,
                            RegistrationKind::Path,
                            "パス",
                        );
                        ui.selectable_value(
                            &mut self.registration_kind,
                            RegistrationKind::Icon,
                            "アイコン",
                        );
                    });
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    self.register_favorite();
                }
                if ui.button("追加").clicked() {
                    self.register_favorite();
                }
                if ui
                    .add_enabled(self.selected_favorite.is_some(), egui::Button::new("削除"))
                    .clicked()
                {
                    self.remove_selected_favorite();
                }
                if ui
                    .add_enabled(self.selected_favorite.is_some(), egui::Button::new("開く"))
                    .clicked()
                {
                    self.open_selected_favorite();
                }
            });
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ScrollArea::vertical().max_height(185.0).show(ui, |ui| {
                    for index in 0..self.favorites.len() {
                        let path = &self.favorites[index];
                        let response =
                            ui.selectable_label(self.selected_favorite == Some(index), path);
                        if response.clicked() {
                            self.selected_favorite = Some(index);
                        }
                        if response.double_clicked() {
                            self.selected_favorite = Some(index);
                            self.open_selected_favorite();
                        }
                    }
                });
            });

            ui.add_space(4.0);
            ui.heading("アイコンお気に入り");
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        self.selected_icon_favorite.is_some(),
                        egui::Button::new("開く"),
                    )
                    .clicked()
                {
                    self.open_selected_icon_favorite();
                }
                if ui
                    .add_enabled(
                        self.selected_icon_favorite.is_some(),
                        egui::Button::new("削除"),
                    )
                    .clicked()
                {
                    self.remove_selected_icon_favorite();
                }
            });
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                    egui::Grid::new("icon_favorites_grid")
                        .num_columns(8)
                        .spacing([10.0, 10.0])
                        .show(ui, |ui| {
                            for index in 0..self.icon_favorites.len() {
                                let path = self.icon_favorites[index].path.clone();
                                self.cache_icon(ctx, &path);
                                let response = match self
                                    .icon_textures
                                    .get(&path)
                                    .and_then(|icon| icon.as_ref())
                                {
                                    Some(icon) => ui.add(
                                        egui::ImageButton::new((icon.id(), egui::vec2(56.0, 56.0)))
                                            .selected(self.selected_icon_favorite == Some(index)),
                                    ),
                                    None => ui.add_sized(
                                        [56.0, 56.0],
                                        egui::Button::new("?")
                                            .selected(self.selected_icon_favorite == Some(index)),
                                    ),
                                };
                                let response = response.on_hover_text(&path);
                                if response.clicked() {
                                    self.selected_icon_favorite = Some(index);
                                }
                                if response.double_clicked() {
                                    self.selected_icon_favorite = Some(index);
                                    self.open_selected_icon_favorite();
                                }
                                if (index + 1) % 8 == 0 {
                                    ui.end_row();
                                }
                            }
                        });
                });
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("ログ");
                let label = if self.log_collapsed {
                    "展開"
                } else {
                    "折りたたむ"
                };
                if ui.button(label).clicked() {
                    self.log_collapsed = !self.log_collapsed;
                }
            });
            if !self.log_collapsed {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ScrollArea::vertical()
                        .max_height(120.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for (is_error, message) in &self.logs {
                                let color = if *is_error {
                                    Color32::RED
                                } else {
                                    ui.visuals().text_color()
                                };
                                let prefix = if *is_error { "[ERROR]" } else { "[INFO ]" };
                                ui.label(RichText::new(format!("{prefix} {message}")).color(color));
                            }
                        });
                });
            }
        });
    }
}

fn add_duplicate_parent_labels(files: &mut [LauncherFile]) {
    let mut label_counts = HashMap::new();
    for file in files.iter() {
        *label_counts.entry(file.label.clone()).or_insert(0_usize) += 1;
    }
    for file in files.iter_mut() {
        if label_counts.get(&file.label).copied().unwrap_or_default() < 2 {
            continue;
        }
        let parent = Path::new(&file.path)
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("…");
        file.label = format!("{} ({parent})", file.label);
    }
}

fn collect_launcher_files(directory: &Path, files: &mut Vec<LauncherFile>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // シンボリックリンクやジャンクションは循環参照を避けるため追跡しない。
        if file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        if file_type.is_dir() {
            collect_launcher_files(&path, files);
        } else if file_type.is_file() {
            files.push(LauncherFile {
                label: default_icon_label(&path.to_string_lossy()),
                path: path.to_string_lossy().into_owned(),
            });
        }
    }
}

fn default_icon_label(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}

fn open_path(path: &str) -> std::io::Result<()> {
    let target = Path::new(path);
    // .code-workspace は関連付け起動が成功扱いでも VS Code に渡らない環境がある。
    // VS Code 本体が見つかる場合は、ワークスペースを引数として直接起動する。
    if target
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("code-workspace"))
    {
        if let Some(code_cli) = vscode_cli() {
            return Command::new("cmd")
                .arg("/C")
                .arg(code_cli)
                .arg("--new-window")
                .arg(target)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map(|_| ());
        }
    }
    open::that(path)
}

fn vscode_cli() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local_app_data)
                .join("Programs")
                .join("Microsoft VS Code")
                .join("bin")
                .join("code.cmd"),
        );
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(program_files) = std::env::var_os(variable) {
            candidates.push(
                PathBuf::from(program_files)
                    .join("Microsoft VS Code")
                    .join("bin")
                    .join("code.cmd"),
            );
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn config_path() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pyclip.json")
}

fn configure_japanese_font(ctx: &egui::Context) {
    // eframe の標準フォントには日本語グリフがないため、Windows 標準の
    // Noto Sans JP を最優先のフォールバックとして登録する。
    let font_path = PathBuf::from(r"C:\Windows\Fonts\NotoSansJP-VF.ttf");
    let Ok(font_bytes) = fs::read(font_path) else {
        return;
    };

    let mut fonts = FontDefinitions::default();
    let font_name = "noto_sans_jp".to_owned();
    fonts.font_data.insert(
        font_name.clone(),
        Arc::new(FontData::from_owned(font_bytes)),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .get_mut(&family)
            .expect("default font family")
            .insert(0, font_name.clone());
    }
    ctx.set_fonts(fonts);
}

fn app_icon() -> egui::IconData {
    let image = image::load_from_memory(include_bytes!("../assets/hand-icon.png"))
        .expect("embedded app icon must be readable")
        .to_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn clipboard_files() -> Vec<String> {
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP) == 0 || OpenClipboard(std::ptr::null_mut()) == 0 {
            return Vec::new();
        }
        let handle = GetClipboardData(CF_HDROP);
        if handle.is_null() {
            CloseClipboard();
            return Vec::new();
        }
        let count = DragQueryFileW(handle, u32::MAX, std::ptr::null_mut(), 0);
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let length = DragQueryFileW(handle, index, std::ptr::null_mut(), 0);
            let mut buffer = vec![0_u16; length as usize + 1];
            DragQueryFileW(handle, index, buffer.as_mut_ptr(), buffer.len() as u32);
            paths.push(String::from_utf16_lossy(&buffer[..length as usize]));
        }
        CloseClipboard();
        paths
    }
}

fn main() -> eframe::Result<()> {
    let window_title = format!("H.A.N.D v{APP_VERSION}");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([980.0, 700.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        &window_title,
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::light());
            configure_japanese_font(&cc.egui_ctx);
            Ok(Box::new(HandApp::new()))
        }),
    )
}
