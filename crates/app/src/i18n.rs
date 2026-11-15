//! All user-visible text, in Ukrainian and English.
//!
//! Every string lives in one [`Strings`] table per language. A struct (not a
//! key → string map) makes a missing translation a compile error, so the
//! Ukrainian UI can never leak an English string. Text with numbers goes
//! through the small helper methods on [`Tr`], which pick the right plural
//! form (Ukrainian has three: 1 файл, 2 файли, 5 файлів) and number
//! formatting from `ringscan_core::format`.
//!
//! The UI reads the active table through [`Tr`], a `Copy` handle created
//! once per frame from the language setting.

use ringscan_core::{Lang, format};

/// The text of one language.
pub struct Strings {
    pub app_name: &'static str,
    pub this_pc: &'static str,
    pub drives: &'static str,
    pub local_disk: &'static str,
    pub choose_folder: &'static str,
    pub choose_folder_tip: &'static str,
    pub recycle_bin: &'static str,
    pub empty_bin: &'static str,
    pub installed_apps: &'static str,
    pub installed_apps_tip: &'static str,
    pub unknown: &'static str,
    pub scan_drive_tip: &'static str,

    pub back_tip: &'static str,
    pub up_tip: &'static str,
    pub rescan_tip: &'static str,
    pub cancel_scan_tip: &'static str,
    pub search_hint: &'static str,
    pub search_tip: &'static str,
    pub clear_search_tip: &'static str,
    pub on_disk: &'static str,
    pub logical: &'static str,
    pub size_mode_tip: &'static str,
    pub colors_folders: &'static str,
    pub colors_types: &'static str,
    pub colors_age: &'static str,
    pub color_mode_tip: &'static str,
    pub settings: &'static str,
    pub language: &'static str,
    pub lang_auto: &'static str,
    pub theme: &'static str,
    pub theme_system: &'static str,
    pub theme_light: &'static str,
    pub theme_dark: &'static str,
    pub show_free: &'static str,
    pub max_depth: &'static str,
    pub min_arc: &'static str,
    pub explorer_menu: &'static str,
    pub explorer_menu_tip: &'static str,
    pub run_as_admin: &'static str,
    pub run_as_admin_tip: &'static str,
    pub panel_toggle_tip: &'static str,
    pub scan_parent_tip: &'static str,

    pub free_space: &'static str,
    pub unaccounted: &'static str,
    pub unaccounted_note: &'static str,
    pub orphaned: &'static str,
    pub reparse_note: &'static str,
    pub error_note: &'static str,
    pub cloud_note: &'static str,
    pub of_parent: &'static str,
    pub of_focus: &'static str,
    pub modified: &'static str,
    pub click_up_tip: &'static str,
    pub empty: &'static str,
    pub nothing_scanned: &'static str,

    pub menu_zoom: &'static str,
    pub menu_open: &'static str,
    pub menu_reveal: &'static str,
    pub menu_copy_path: &'static str,
    pub menu_rescan: &'static str,
    pub menu_recycle: &'static str,
    pub menu_delete: &'static str,
    pub menu_properties: &'static str,

    pub tab_contents: &'static str,
    pub tab_largest: &'static str,
    pub tab_types: &'static str,
    pub col_name: &'static str,
    pub col_size: &'static str,
    pub col_percent: &'static str,
    pub col_files: &'static str,
    pub col_modified: &'static str,
    pub no_files: &'static str,
    pub computing: &'static str,
    pub no_ext: &'static str,
    pub nothing_found: &'static str,
    pub search_results: &'static str,

    pub st_scanning: &'static str,
    pub st_done: &'static str,
    pub st_cancelled: &'static str,
    pub st_idle: &'static str,
    pub bg_stop_tip: &'static str,
    pub method_walk: &'static str,
    pub method_demo: &'static str,
    pub method_mft: &'static str,
    pub method_mixed: &'static str,
    pub method_pending: &'static str,
    pub method_tip_admin: &'static str,
    pub note_not_elevated: &'static str,
    pub focus: &'static str,

    pub dlg_recycle_title: &'static str,
    pub dlg_delete_title: &'static str,
    pub dlg_delete_warning: &'static str,
    pub dlg_empty_title: &'static str,
    pub dlg_empty_text: &'static str,
    pub cancel: &'static str,
    pub btn_recycle: &'static str,
    pub btn_delete: &'static str,
    pub btn_empty: &'static str,
    pub total: &'static str,

    pub toast_copied: &'static str,
    pub toast_not_recyclable: &'static str,
    pub toast_script_skipped: &'static str,
    pub toast_error: &'static str,
    pub toast_not_found: &'static str,
    pub toast_recycled: &'static str,
    pub toast_deleted: &'static str,
    pub toast_busy: &'static str,

    pub grp_video: &'static str,
    pub grp_audio: &'static str,
    pub grp_images: &'static str,
    pub grp_archives: &'static str,
    pub grp_programs: &'static str,
    pub grp_documents: &'static str,
    pub grp_code: &'static str,
    pub grp_system: &'static str,
    pub grp_other: &'static str,

    pub age_day: &'static str,
    pub age_week: &'static str,
    pub age_month: &'static str,
    pub age_year: &'static str,
    pub age_3y: &'static str,
    pub age_10y: &'static str,

    pub month_short: [&'static str; 12],
}

pub static UK: Strings = Strings {
    app_name: "RingScan",
    this_pc: "Цей ПК",
    drives: "Диски",
    local_disk: "Локальний диск",
    choose_folder: "Вибрати папку…",
    choose_folder_tip: "Сканувати будь-яку папку",
    recycle_bin: "Кошик",
    empty_bin: "Очистити…",
    installed_apps: "Встановлені програми",
    installed_apps_tip: "Відкрити «Параметри › Програми › Встановлені програми»",
    unknown: "невідомо",
    scan_drive_tip: "Сканувати диск",

    back_tip: "Назад (Alt+←, Backspace)",
    up_tip: "На рівень вище (Alt+↑)",
    rescan_tip: "Пересканувати поточну папку (F5)",
    cancel_scan_tip: "Зупинити сканування",
    search_hint: "Пошук",
    search_tip: "Пошук за назвою в поточній папці (Ctrl+F)",
    clear_search_tip: "Очистити пошук (Esc)",
    on_disk: "На диску",
    logical: "Логічний",
    size_mode_tip: "Розмір на диску (з урахуванням стиснення) або логічний розмір файлів",
    colors_folders: "Папки",
    colors_types: "Типи",
    colors_age: "Вік",
    color_mode_tip: "Розфарбування діаграми",
    settings: "Налаштування",
    language: "Мова",
    lang_auto: "Як у системі",
    theme: "Тема",
    theme_system: "Як у системі",
    theme_light: "Світла",
    theme_dark: "Темна",
    show_free: "Показувати вільне місце",
    max_depth: "Кількість кілець",
    min_arc: "Найменша дуга",
    explorer_menu: "Пункт у контекстному меню Провідника",
    explorer_menu_tip: "«Відкрити в RingScan» для папок і дисків (лише для поточного користувача)",
    run_as_admin: "Перезапустити як адміністратор",
    run_as_admin_tip: "Швидке сканування NTFS через MFT потребує прав адміністратора",
    panel_toggle_tip: "Показати або сховати панель",
    scan_parent_tip: "Сканувати цю папку",

    free_space: "Вільне місце",
    unaccounted: "Неврахований простір",
    unaccounted_note: "Зайняте місце, яке не вдалося просканувати (системні або недоступні дані)",
    orphaned: "Файли без папки",
    reparse_note: "Точка з'єднання або символьне посилання — вміст не враховано",
    error_note: "Не вдалося прочитати повністю",
    cloud_note: "Хмарний файл (враховано лише локальну частину)",
    of_parent: "від батьківської",
    of_focus: "від поточної",
    modified: "Змінено",
    click_up_tip: "Клацніть, щоб піднятися на рівень вище",
    empty: "Порожньо",
    nothing_scanned: "Виберіть диск або папку ліворуч",

    menu_zoom: "Наблизити",
    menu_open: "Відкрити",
    menu_reveal: "Показати в Провіднику",
    menu_copy_path: "Копіювати шлях",
    menu_rescan: "Пересканувати",
    menu_recycle: "Перемістити в кошик…",
    menu_delete: "Видалити назавжди…",
    menu_properties: "Властивості",

    tab_contents: "Вміст",
    tab_largest: "Найбільші файли",
    tab_types: "Типи файлів",
    col_name: "Назва",
    col_size: "Розмір",
    col_percent: "Частка",
    col_files: "Файлів",
    col_modified: "Змінено",
    no_files: "Тут немає файлів",
    computing: "Обчислення…",
    no_ext: "(без розширення)",
    nothing_found: "Нічого не знайдено",
    search_results: "Результати пошуку",

    st_scanning: "Сканування",
    st_done: "Готово",
    st_cancelled: "Зупинено",
    st_idle: "Очікування",
    bg_stop_tip: "Зупинити фонові сканування",
    method_walk: "Обхід папок",
    method_demo: "Демонстраційні дані",
    method_mft: "Таблиця MFT",
    method_mixed: "MFT + обхід папок",
    method_pending: "Підготовка",
    method_tip_admin: "Швидке читання MFT потребує прав адміністратора: «Перезапустити як адміністратор» у налаштуваннях",
    note_not_elevated: "без прав адміністратора MFT недоступна, тому диск прочитано обходом папок",
    focus: "Поточна папка",

    dlg_recycle_title: "Перемістити в кошик?",
    dlg_delete_title: "Видалити назавжди?",
    dlg_delete_warning: "Файли буде видалено без кошика. Цю дію не можна скасувати.",
    dlg_empty_title: "Очистити кошик?",
    dlg_empty_text: "Усі об'єкти в кошику буде видалено назавжди.",
    cancel: "Скасувати",
    btn_recycle: "Перемістити в кошик",
    btn_delete: "Видалити назавжди",
    btn_empty: "Очистити кошик",
    total: "Разом",

    toast_copied: "Шлях скопійовано",
    toast_not_recyclable: "Не можна перемістити в кошик (лише видалити остаточно)",
    toast_script_skipped: "Пропущено в режимі сценарію",
    toast_error: "Помилка",
    toast_not_found: "Шлях не знайдено",
    toast_recycled: "Переміщено в кошик",
    toast_deleted: "Видалено",
    toast_busy: "Дочекайтеся завершення сканування",

    grp_video: "Відео",
    grp_audio: "Аудіо",
    grp_images: "Зображення",
    grp_archives: "Архіви та образи",
    grp_programs: "Програми",
    grp_documents: "Документи",
    grp_code: "Код і дані",
    grp_system: "Системні",
    grp_other: "Інше",

    age_day: "сьогодні",
    age_week: "тиждень",
    age_month: "місяць",
    age_year: "рік",
    age_3y: "3 роки",
    age_10y: "10 років",

    month_short: [
        "січ.",
        "лют.",
        "бер.",
        "квіт.",
        "трав.",
        "черв.",
        "лип.",
        "серп.",
        "вер.",
        "жовт.",
        "лист.",
        "груд.",
    ],
};

pub static EN: Strings = Strings {
    app_name: "RingScan",
    this_pc: "This PC",
    drives: "Drives",
    local_disk: "Local Disk",
    choose_folder: "Choose folder…",
    choose_folder_tip: "Scan any folder",
    recycle_bin: "Recycle Bin",
    empty_bin: "Empty…",
    installed_apps: "Installed apps",
    installed_apps_tip: "Open Settings › Apps › Installed apps",
    unknown: "unknown",
    scan_drive_tip: "Scan drive",

    back_tip: "Back (Alt+Left, Backspace)",
    up_tip: "Up one level (Alt+Up)",
    rescan_tip: "Rescan the current folder (F5)",
    cancel_scan_tip: "Stop scanning",
    search_hint: "Search",
    search_tip: "Search by name in the current folder (Ctrl+F)",
    clear_search_tip: "Clear search (Esc)",
    on_disk: "On disk",
    logical: "Logical",
    size_mode_tip: "Size on disk (compression aware) or logical file size",
    colors_folders: "Folders",
    colors_types: "File types",
    colors_age: "Age",
    color_mode_tip: "Chart colors",
    settings: "Settings",
    language: "Language",
    lang_auto: "System default",
    theme: "Theme",
    theme_system: "System default",
    theme_light: "Light",
    theme_dark: "Dark",
    show_free: "Show free space",
    max_depth: "Rings",
    min_arc: "Smallest arc",
    explorer_menu: "Explorer context menu entry",
    explorer_menu_tip: "\"Open in RingScan\" for folders and drives (current user only)",
    run_as_admin: "Restart as administrator",
    run_as_admin_tip: "The fast NTFS scan reads the MFT and needs administrator rights",
    panel_toggle_tip: "Show or hide the panel",
    scan_parent_tip: "Scan this folder",

    free_space: "Free space",
    unaccounted: "Unaccounted",
    unaccounted_note: "Used space the scan could not see (system or inaccessible data)",
    orphaned: "Orphaned files",
    reparse_note: "Junction or symbolic link: contents not counted",
    error_note: "Could not be read completely",
    cloud_note: "Cloud file (only the local part is counted)",
    of_parent: "of parent",
    of_focus: "of current",
    modified: "Modified",
    click_up_tip: "Click to go up one level",
    empty: "Empty",
    nothing_scanned: "Pick a drive or a folder on the left",

    menu_zoom: "Zoom in",
    menu_open: "Open",
    menu_reveal: "Show in Explorer",
    menu_copy_path: "Copy path",
    menu_rescan: "Rescan",
    menu_recycle: "Move to Recycle Bin…",
    menu_delete: "Delete permanently…",
    menu_properties: "Properties",

    tab_contents: "Contents",
    tab_largest: "Largest files",
    tab_types: "File types",
    col_name: "Name",
    col_size: "Size",
    col_percent: "Share",
    col_files: "Files",
    col_modified: "Modified",
    no_files: "No files here",
    computing: "Computing…",
    no_ext: "(no extension)",
    nothing_found: "Nothing found",
    search_results: "Search results",

    st_scanning: "Scanning",
    st_done: "Done",
    st_cancelled: "Stopped",
    st_idle: "Idle",
    bg_stop_tip: "Stop the background scans",
    method_walk: "Folder walk",
    method_demo: "Demo data",
    method_mft: "MFT",
    method_mixed: "MFT + folder walk",
    method_pending: "Preparing",
    method_tip_admin: "Reading the MFT (fast) needs administrator rights: \"Restart as administrator\" in the settings",
    note_not_elevated: "not elevated: the MFT cannot be read, so the drive was walked",
    focus: "Current folder",

    dlg_recycle_title: "Move to Recycle Bin?",
    dlg_delete_title: "Delete permanently?",
    dlg_delete_warning: "The files will bypass the Recycle Bin. This cannot be undone.",
    dlg_empty_title: "Empty the Recycle Bin?",
    dlg_empty_text: "All items in the Recycle Bin will be deleted permanently.",
    cancel: "Cancel",
    btn_recycle: "Move to Recycle Bin",
    btn_delete: "Delete permanently",
    btn_empty: "Empty Recycle Bin",
    total: "Total",

    toast_copied: "Path copied",
    toast_not_recyclable: "Cannot be moved to the Recycle Bin (only deleted permanently)",
    toast_script_skipped: "Skipped in script mode",
    toast_error: "Error",
    toast_not_found: "Path not found",
    toast_recycled: "Moved to the Recycle Bin",
    toast_deleted: "Deleted",
    toast_busy: "Wait until the scan finishes",

    grp_video: "Video",
    grp_audio: "Audio",
    grp_images: "Images",
    grp_archives: "Archives & images",
    grp_programs: "Programs",
    grp_documents: "Documents",
    grp_code: "Code & data",
    grp_system: "System",
    grp_other: "Other",

    age_day: "today",
    age_week: "week",
    age_month: "month",
    age_year: "year",
    age_3y: "3 years",
    age_10y: "10 years",

    month_short: [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ],
};

/// Handle to the active language: the string table plus number helpers.
#[derive(Clone, Copy)]
pub struct Tr {
    pub lang: Lang,
    pub s: &'static Strings,
}

/// Ukrainian plural category of `n`: 0 = one (1, 21), 1 = few (2–4, 22–24),
/// 2 = many (0, 5–20, 25–30, ...).
fn uk_plural(n: u64) -> usize {
    let (m10, m100) = (n % 10, n % 100);
    if m10 == 1 && m100 != 11 {
        0
    } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
        1
    } else {
        2
    }
}

impl Tr {
    pub fn new(lang: Lang) -> Tr {
        Tr {
            lang,
            s: match lang {
                Lang::Uk => &UK,
                Lang::En => &EN,
            },
        }
    }

    pub fn size(&self, bytes: u64) -> String {
        format::size(bytes, self.lang)
    }

    pub fn count(&self, n: u64) -> String {
        format::count(n, self.lang)
    }

    pub fn percent(&self, ratio: f64) -> String {
        format::percent(ratio, self.lang)
    }

    /// "N noun" with the right plural form.
    fn counted(&self, n: u64, uk: [&str; 3], en: [&str; 2]) -> String {
        let word = match self.lang {
            Lang::Uk => uk[uk_plural(n)],
            Lang::En => en[usize::from(n != 1)],
        };
        format!("{} {}", self.count(n), word)
    }

    pub fn n_files(&self, n: u64) -> String {
        self.counted(n, ["файл", "файли", "файлів"], ["file", "files"])
    }

    pub fn n_folders(&self, n: u64) -> String {
        self.counted(n, ["папка", "папки", "папок"], ["folder", "folders"])
    }

    pub fn n_items(&self, n: u64) -> String {
        self.counted(n, ["об'єкт", "об'єкти", "об'єктів"], ["item", "items"])
    }

    /// Tooltip title of a Rest arc.
    pub fn n_smaller_items(&self, n: u64) -> String {
        self.counted(
            n,
            [
                "дрібніший елемент",
                "дрібніші елементи",
                "дрібніших елементів",
            ],
            ["smaller item", "smaller items"],
        )
    }

    pub fn n_drives(&self, n: u64) -> String {
        self.counted(n, ["диск", "диски", "дисків"], ["drive", "drives"])
    }

    pub fn and_more(&self, n: u64) -> String {
        match self.lang {
            Lang::Uk => format!("…і ще {}", self.count(n)),
            Lang::En => format!("…and {} more", self.count(n)),
        }
    }

    pub fn free_of(&self, free: u64, total: u64) -> String {
        match self.lang {
            Lang::Uk => format!("{} вільно з {}", self.size(free), self.size(total)),
            Lang::En => format!("{} free of {}", self.size(free), self.size(total)),
        }
    }

    pub fn used(&self, used: u64) -> String {
        match self.lang {
            Lang::Uk => format!("зайнято {}", self.size(used)),
            Lang::En => format!("{} used", self.size(used)),
        }
    }

    pub fn files_per_sec(&self, rate: u64) -> String {
        match self.lang {
            Lang::Uk => format!("{} файлів/с", self.count(rate)),
            Lang::En => format!("{} files/s", self.count(rate)),
        }
    }

    /// "MFT 45 %" while the master file table is read.
    pub fn mft_progress(&self, done: u64, total: u64) -> String {
        let ratio = if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        format!("MFT {}", self.percent(ratio.min(1.0)))
    }

    /// Status bar: scans still running in the background (in memory, not
    /// shown).
    pub fn n_background(&self, n: u64) -> String {
        self.counted(
            n,
            ["сканування у фоні", "сканування у фоні", "сканувань у фоні"],
            ["scan in the background", "scans in the background"],
        )
    }

    pub fn n_errors(&self, n: u64) -> String {
        self.counted(n, ["помилка", "помилки", "помилок"], ["error", "errors"])
    }

    pub fn threads(&self, n: usize) -> String {
        self.counted(
            n as u64,
            ["потік", "потоки", "потоків"],
            ["thread", "threads"],
        )
    }

    pub fn n_results(&self, n: u64, capped: bool) -> String {
        let plus = if capped { "+" } else { "" };
        format!("{}{plus}", self.count(n))
    }

    /// "12 бер. 2024" / "Mar 12, 2024" for a unix time (UTC date).
    pub fn date(&self, unix: i64) -> String {
        if unix <= 0 {
            return "—".into();
        }
        let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
        let mon = self.s.month_short[(m - 1) as usize];
        match self.lang {
            Lang::Uk => format!("{d} {mon} {y}"),
            Lang::En => format!("{mon} {d}, {y}"),
        }
    }

    /// Compact numeric date for table cells: "12.03.2024" / "2024-03-12".
    pub fn short_date(&self, unix: i64) -> String {
        if unix <= 0 {
            return "—".into();
        }
        let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
        match self.lang {
            Lang::Uk => format!("{d:02}.{m:02}.{y}"),
            Lang::En => format!("{y}-{m:02}-{d:02}"),
        }
    }

    /// "1:05" / "12:34:56" elapsed time.
    pub fn duration(&self, secs: u64) -> String {
        let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m}:{s:02}")
        }
    }
}

/// Days since 1970-01-01 → (year, month 1..=12, day 1..=31), proleptic
/// Gregorian (Howard Hinnant's `civil_from_days`).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plurals() {
        let t = Tr::new(Lang::Uk);
        assert_eq!(t.n_files(1), "1 файл");
        assert_eq!(t.n_files(3), "3 файли");
        assert_eq!(t.n_files(11), "11 файлів");
        assert_eq!(t.n_files(21), "21 файл");
        assert_eq!(t.n_files(112), "112 файлів");
        let e = Tr::new(Lang::En);
        assert_eq!(e.n_files(1), "1 file");
        assert_eq!(e.n_files(2), "2 files");
        assert_eq!(t.n_background(2), "2 сканування у фоні");
        assert_eq!(t.n_background(5), "5 сканувань у фоні");
        assert_eq!(e.n_background(1), "1 scan in the background");
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-11-14
        assert_eq!(civil_from_days(20_771), (2026, 11, 14));
        let t = Tr::new(Lang::Uk);
        assert_eq!(t.short_date(1_767_225_600), "01.01.2026");
    }
}
