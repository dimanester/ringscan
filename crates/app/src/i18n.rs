//! All user-visible text, in Ukrainian and English.
//!
//! Every string lives in one [`Strings`] table per language. A struct (not a
//! key → string map) makes a missing translation a compile error, so the
//! Ukrainian UI can never leak an English string. Text with numbers goes
//! through the small helper methods on [`Tr`], which pick the right plural
//! form (Ukrainian has three: 1 файл, 2 файли, 5 файлів) and number
//! formatting from `ringscan_core::format`.
//!
//! Counts are grouped with a no-break space (U+00A0) in Ukrainian, as
//! Windows does for uk-UA, not with the narrow one (U+202F): in Segoe UI
//! Variable the narrow space is 1.7 px wide at 12.5 px, no wider than the
//! side bearings of a "1" or "7", so `1 137 193` read as `1137193`.
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

    pub support: &'static str,
    pub support_menu: &'static str,
    pub support_title: &'static str,
    pub support_text: &'static str,
    pub support_jar: &'static str,
    pub support_jar_sub: &'static str,
    pub support_usdt_warn: &'static str,
    pub support_btc_warn: &'static str,
    pub support_copy: &'static str,
    pub support_copied: &'static str,
    pub support_all: &'static str,
    pub close: &'static str,

    pub free_space: &'static str,
    pub unaccounted: &'static str,
    pub unaccounted_note: &'static str,
    pub reserved: &'static str,
    pub reserved_note: &'static str,
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
    pub note_no_drive_info: &'static str,
    pub note_folder_only: &'static str,
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
    pub toast_item_changed: &'static str,
    pub toast_open_link_failed: &'static str,

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

    support: "Підтримати",
    support_menu: "Підтримати RingScan…",
    support_title: "Підтримати RingScan",
    support_text: "RingScan безкоштовний і з відкритим кодом. Якщо він вам допоміг, можна підтримати розробку.",
    support_jar: "Банка Monobank",
    support_jar_sub: "Картка, Apple Pay, Google Pay з будь-якої країни",
    support_usdt_warn: "Лише мережа TRON (TRC-20)",
    support_btc_warn: "Лише мережа Bitcoin",
    support_copy: "Копіювати адресу",
    support_copied: "Адресу скопійовано",
    support_all: "Усі способи на GitHub",
    close: "Закрити",

    free_space: "Вільне місце",
    unaccounted: "Неврахований простір",
    unaccounted_note: "Зайняте місце, яке не вдалося просканувати (системні або недоступні дані)",
    reserved: "Резерв Windows",
    reserved_note: "Місце, яке Windows тримає про запас (для оновлень і потреб NTFS); файлів у ньому немає",
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
    note_no_drive_info: "не вдалося отримати відомості про диск, тому його прочитано обходом папок",
    note_folder_only: "MFT читається лише для цілого диска, тому папку прочитано обходом",
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
    toast_item_changed: "Елемент змінився після вибору (його пересканували), тому нічого не видалено. Спробуйте ще раз",
    toast_open_link_failed: "Не вдалося відкрити посилання",

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

    support: "Support",
    support_menu: "Support RingScan…",
    support_title: "Support RingScan",
    support_text: "RingScan is free and open source. If it helped you, you can support its development.",
    support_jar: "Monobank jar",
    support_jar_sub: "Card, Apple Pay, Google Pay - any country",
    support_usdt_warn: "Only the TRON (TRC-20) network",
    support_btc_warn: "Only the Bitcoin network",
    support_copy: "Copy address",
    support_copied: "Address copied",
    support_all: "All options on GitHub",
    close: "Close",

    free_space: "Free space",
    unaccounted: "Unaccounted",
    unaccounted_note: "Used space the scan could not see (system or inaccessible data)",
    reserved: "Reserved by Windows",
    reserved_note: "Space Windows keeps in reserve (for updates and NTFS); no files are in it",
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
    note_no_drive_info: "the drive information could not be read, so the drive was walked",
    note_folder_only: "the MFT is read for whole drives only, so the folder was walked",
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
    toast_item_changed: "The item changed after it was selected (it was rescanned), so nothing was deleted. Try again",
    toast_open_link_failed: "Could not open the link",

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

    /// `1 234 567` (no-break spaces) / `1,234,567`; see the module docs.
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

    /// Free space under the size in the chart's center disc.
    pub fn free(&self, free: u64) -> String {
        match self.lang {
            Lang::Uk => format!("вільно {}", self.size(free)),
            Lang::En => format!("{} free", self.size(free)),
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

    /// The engine's reason for walking a drive (or for an MFT failure), in
    /// the UI language.
    pub fn unit_note(&self, note: &ringscan_engine::UnitNote) -> String {
        use ringscan_engine::UnitNote;
        match note {
            UnitNote::NotElevated => self.s.note_not_elevated.to_string(),
            UnitNote::NoDriveInfo => self.s.note_no_drive_info.to_string(),
            UnitNote::FolderOnly => self.s.note_folder_only.to_string(),
            UnitNote::NotNtfs { letter, fs } => match (self.lang, fs.is_empty()) {
                (Lang::Uk, false) => {
                    format!(
                        "{letter}: має файлову систему {fs}, а не NTFS, тому диск прочитано обходом папок"
                    )
                }
                (Lang::Uk, true) => {
                    format!(
                        "{letter}: має невідому файлову систему, тому диск прочитано обходом папок"
                    )
                }
                (Lang::En, false) => {
                    format!("{letter}: is {fs}, not NTFS, so the drive was walked")
                }
                (Lang::En, true) => {
                    format!("{letter}: has an unknown file system, so the drive was walked")
                }
            },
            // The error text comes from the OS, in its language.
            UnitNote::MftFailed(e) => match self.lang {
                Lang::Uk => {
                    format!("не вдалося прочитати MFT ({e}), тому диск прочитано обходом папок")
                }
                Lang::En => format!("the MFT could not be read ({e}), so the drive was walked"),
            },
        }
    }

    /// Toast for a link the shell could not open, with ShellExecute's
    /// error code when there is one.
    pub fn open_link_failed(&self, code: Option<u32>) -> String {
        let s = self.s.toast_open_link_failed;
        match (code, self.lang) {
            (Some(c), Lang::Uk) => format!("{s} (код {c})"),
            (Some(c), Lang::En) => format!("{s} (code {c})"),
            (None, _) => s.to_string(),
        }
    }

    /// "0,30°" / "0.30°": an angle with two decimals (the settings' smallest
    /// arc).
    pub fn degrees(&self, v: f64) -> String {
        let number = format!("{v:.2}");
        match self.lang {
            Lang::Uk => format!("{}°", number.replace('.', ",")),
            Lang::En => format!("{number}°"),
        }
    }

    /// A number typed into a slider's text field, with a decimal comma or
    /// point and an optional unit sign: "0,3", "0.3°", " 1,5 ° ".
    pub fn parse_decimal(text: &str) -> Option<f64> {
        let t = text.trim().trim_end_matches('°').trim().replace(',', ".");
        t.parse().ok().filter(|v: &f64| v.is_finite())
    }

    /// "12 бер. 2024" / "Mar 12, 2024" for a *local* wall-clock time in
    /// seconds since 1970 (`platform::local_time` of a unix time), so the
    /// day is the one Explorer shows.
    pub fn date(&self, local: i64) -> String {
        if local <= 0 {
            return "—".into();
        }
        let (y, m, d) = civil_from_days(local.div_euclid(86_400));
        let mon = self.s.month_short[(m - 1) as usize];
        match self.lang {
            Lang::Uk => format!("{d} {mon} {y}"),
            Lang::En => format!("{mon} {d}, {y}"),
        }
    }

    /// Compact numeric date for table cells: "12.03.2024" / "2024-03-12";
    /// takes a local time like [`Tr::date`].
    pub fn short_date(&self, local: i64) -> String {
        if local <= 0 {
            return "—".into();
        }
        let (y, m, d) = civil_from_days(local.div_euclid(86_400));
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

    /// Every engine note reads in the UI language (the Ukrainian UI used to
    /// show all but one of them in English).
    #[test]
    fn unit_notes_are_localized() {
        use ringscan_engine::UnitNote;
        let notes = [
            UnitNote::NotElevated,
            UnitNote::NotNtfs {
                letter: 'G',
                fs: "FAT32".into(),
            },
            UnitNote::NotNtfs {
                letter: 'E',
                fs: String::new(),
            },
            UnitNote::NoDriveInfo,
            UnitNote::MftFailed("Відмовлено в доступі. (os error 5)".into()),
            UnitNote::FolderOnly,
        ];
        let (uk, en) = (Tr::new(Lang::Uk), Tr::new(Lang::En));
        for n in &notes {
            let english = n.to_string();
            let u = uk.unit_note(n);
            assert!(u.chars().any(|c| ('а'..='я').contains(&c)), "{u}");
            assert!(!u.contains("walked") && !u.contains("not NTFS"), "{u}");
            assert_ne!(u, english);
            assert!(en.unit_note(n).is_ascii() || matches!(n, UnitNote::MftFailed(_)));
        }
        assert!(uk.unit_note(&notes[1]).starts_with("G: "));
        assert!(uk.unit_note(&notes[1]).contains("FAT32"));
        assert_eq!(
            uk.open_link_failed(Some(31)),
            "Не вдалося відкрити посилання (код 31)"
        );
        assert_eq!(en.open_link_failed(None), "Could not open the link");
    }

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

    /// Every count helper groups thousands, in every plural form (the
    /// "few" form, 2-4 / 22-24 but not 12-14, once looked ungrouped).
    #[test]
    fn counts_grouped_in_all_plural_forms() {
        let (uk, en) = (Tr::new(Lang::Uk), Tr::new(Lang::En));
        let cases: [(u64, &str, &str, &str); 16] = [
            (0, "0", "файлів", "files"),
            (1, "1", "файл", "file"),
            (2, "2", "файли", "files"),
            (4, "4", "файли", "files"),
            (5, "5", "файлів", "files"),
            (11, "11", "файлів", "files"),
            (12, "12", "файлів", "files"),
            (14, "14", "файлів", "files"),
            (21, "21", "файл", "files"),
            (22, "22", "файли", "files"),
            (25, "25", "файлів", "files"),
            (1_001, "1 001", "файл", "files"),
            (909_535, "909 535", "файлів", "files"),
            (917_152, "917 152", "файли", "files"),
            (1_137_193, "1 137 193", "файли", "files"),
            (1_000_000_000_012, "1 000 000 000 012", "файлів", "files"),
        ];
        for (n, digits, uk_word, en_word) in cases {
            let uk_num = digits.replace(' ', "\u{A0}");
            let en_num = digits.replace(' ', ",");
            assert_eq!(uk.count(n), uk_num, "{n}");
            assert_eq!(en.count(n), en_num, "{n}");
            assert_eq!(uk.n_files(n), format!("{uk_num} {uk_word}"), "{n}");
            assert_eq!(en.n_files(n), format!("{en_num} {en_word}"), "{n}");
            // The other helpers share the number.
            for text in [
                uk.n_folders(n),
                uk.n_items(n),
                uk.n_smaller_items(n),
                uk.n_drives(n),
                uk.n_errors(n),
                uk.n_background(n),
                uk.files_per_sec(n),
                uk.and_more(n),
                uk.n_results(n, true),
            ] {
                assert!(text.contains(&uk_num), "{text:?}");
                assert!(!text.contains('\u{202F}'), "{text:?}");
            }
            for text in [
                en.n_folders(n),
                en.n_items(n),
                en.n_smaller_items(n),
                en.n_drives(n),
                en.n_errors(n),
                en.n_background(n),
                en.files_per_sec(n),
                en.and_more(n),
                en.n_results(n, false),
            ] {
                assert!(text.contains(&en_num), "{text:?}");
            }
        }
        assert_eq!(uk.n_folders(316_454), "316\u{A0}454 папки");
        assert_eq!(uk.n_folders(1_000_001), "1\u{A0}000\u{A0}001 папка");
        assert_eq!(uk.threads(12), "12 потоків");
        assert_eq!(en.threads(1), "1 thread");
    }

    #[test]
    fn free_text() {
        let gb = 1u64 << 30;
        assert_eq!(Tr::new(Lang::Uk).free(1178 * gb), "вільно 1,15 ТБ");
        assert_eq!(Tr::new(Lang::En).free(1178 * gb), "1.15 TB free");
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-11-14
        assert_eq!(civil_from_days(20_771), (2026, 11, 14));
        let t = Tr::new(Lang::Uk);
        assert_eq!(t.short_date(1_767_225_600), "01.01.2026");
    }

    #[test]
    fn dates_take_local_time() {
        // 2026-11-14 22:30 UTC is 2026-11-15 00:30 in Kyiv (UTC+2 in
        // winter): Explorer shows the 15th, and so must the date column
        // and the tooltip once the caller applied the zone offset.
        let utc = 1_794_695_400;
        let kyiv = utc + 7_200;
        let (uk, en) = (Tr::new(Lang::Uk), Tr::new(Lang::En));
        assert_eq!(uk.short_date(utc), "14.11.2026");
        assert_eq!(uk.short_date(kyiv), "15.11.2026");
        assert_eq!(en.short_date(kyiv), "2026-11-15");
        assert_eq!(uk.date(kyiv), format!("15 {} 2026", UK.month_short[10]));
        assert_eq!(en.date(kyiv), "Nov 15, 2026");
    }

    #[test]
    fn degrees_use_the_decimal_separator() {
        assert_eq!(Tr::new(Lang::Uk).degrees(0.3), "0,30°");
        assert_eq!(Tr::new(Lang::En).degrees(0.3), "0.30°");
        assert_eq!(Tr::new(Lang::Uk).degrees(3.0), "3,00°");
        for text in ["0,3", "0.3", "0,3°", " 0.30 ° "] {
            assert_eq!(Tr::parse_decimal(text), Some(0.3), "{text}");
        }
        assert_eq!(Tr::parse_decimal("abc"), None);
        assert_eq!(Tr::parse_decimal("NaN"), None);
    }
}

/// The separator has to be visible in the real UI font (Windows only:
/// the fonts come from `%WINDIR%\Fonts`).
#[cfg(all(test, windows))]
mod font_tests {
    use eframe::egui::{self, FontId};

    #[test]
    fn group_separator_is_visible_in_the_ui_font() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut out = ctx.run_ui(Default::default(), |_| {});
        out.textures_delta.clear();
        let tr = super::Tr::new(ringscan_core::Lang::Uk);
        let sep = tr.count(1_000).chars().nth(1).expect("grouped");
        for font in [FontId::proportional(12.5), crate::theme::semibold(13.0)] {
            ctx.fonts_mut(|f| {
                assert!(f.has_glyph(&font, sep));
                let (w_sep, w_digit) = (f.glyph_width(&font, sep), f.glyph_width(&font, '0'));
                // The narrow no-break space was a quarter of a digit (1.7 px
                // at 12.5 px); a space is about half.
                assert!(w_sep >= 0.45 * w_digit, "{w_sep} vs digit {w_digit}");
            });
        }
    }
}
