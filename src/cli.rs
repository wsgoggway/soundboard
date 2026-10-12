//! Clap definitions for the soundboard command-line interface.
//! Help texts shown to the user are in Russian; identifiers stay English.

use clap::Parser;
use clap::Subcommand;

/// Soundboard: воспроизводит короткие аудиоклипы в виртуальный микрофон
/// PipeWire для видеозвонков (Zoom, Google Meet, Telegram, Jitsi — любых
/// приложений связи) в Linux.
#[derive(Parser, Debug)]
#[command(name = "soundboard", version, about, long_about = None)]
pub struct Cli {
    /// Подкоманда; без неё запускается графический интерфейс.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Available subcommands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Создать или починить аудиограф (идемпотентный reconcile).
    Up,
    /// Выгрузить модули soundboard (чужие модули не трогаются).
    Down,
    /// Проверить состояние аудиографа; код 1 — если найдены проблемы.
    Doctor,
    /// Показать клипы из настроенного каталога клипов.
    List,
    /// Проиграть клип (неблокирующе; граф поднимается автоматически).
    Play {
        /// Имя клипа: точное, уникальное без учёта регистра или уникальный префикс.
        name: String,
    },
    /// Горячие клавиши для клипов: таблица и инструкция для KDE Plasma 6.
    Hotkeys {
        /// Записать .desktop-файлы для автопривязки клавиш (без правки
        /// конфигов KDE; активация — командой из вывода).
        #[arg(long)]
        write: bool,
    },
    /// Остановить воспроизведение; без ИМЕНИ — всё (граф поднимается сначала).
    Stop {
        /// Имя клипа; опустите, чтобы остановить всё.
        name: Option<String>,
    },
}
