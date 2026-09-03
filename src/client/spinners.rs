//! Compact text spinners adapted from FGRibreau/spinners (MIT).
//! Emoji and unusually large/high-frequency styles are intentionally omitted.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SpinnerStyle {
    pub name: &'static str,
    pub frames: &'static [&'static str],
    pub interval_ms: u64,
}

impl SpinnerStyle {
    pub fn frame(self, elapsed_ms: usize) -> &'static str {
        self.frames[(elapsed_ms / self.interval_ms as usize) % self.frames.len()]
    }
}

const S0: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const S1: &[&str] = &["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];
const S2: &[&str] = &["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"];
const S3: &[&str] = &[
    "⠄", "⠆", "⠇", "⠋", "⠙", "⠸", "⠰", "⠠", "⠰", "⠸", "⠙", "⠋", "⠇", "⠆",
];
const S4: &[&str] = &[
    "⠋", "⠙", "⠚", "⠒", "⠂", "⠂", "⠒", "⠲", "⠴", "⠦", "⠖", "⠒", "⠐", "⠐", "⠒", "⠓", "⠋",
];
const S5: &[&str] = &[
    "⠁", "⠉", "⠙", "⠚", "⠒", "⠂", "⠂", "⠒", "⠲", "⠴", "⠤", "⠄", "⠄", "⠤", "⠴", "⠲", "⠒", "⠂", "⠂",
    "⠒", "⠚", "⠙", "⠉", "⠁",
];
const S6: &[&str] = &[
    "⠈", "⠉", "⠋", "⠓", "⠒", "⠐", "⠐", "⠒", "⠖", "⠦", "⠤", "⠠", "⠠", "⠤", "⠦", "⠖", "⠒", "⠐", "⠐",
    "⠒", "⠓", "⠋", "⠉", "⠈",
];
const S7: &[&str] = &[
    "⠁", "⠁", "⠉", "⠙", "⠚", "⠒", "⠂", "⠂", "⠒", "⠲", "⠴", "⠤", "⠄", "⠄", "⠤", "⠠", "⠠", "⠤", "⠦",
    "⠖", "⠒", "⠐", "⠐", "⠒", "⠓", "⠋", "⠉", "⠈", "⠈",
];
const S8: &[&str] = &["⢹", "⢺", "⢼", "⣸", "⣇", "⡧", "⡗", "⡏"];
const S9: &[&str] = &["⢄", "⢂", "⢁", "⡁", "⡈", "⡐", "⡠"];
const S10: &[&str] = &["⠁", "⠂", "⠄", "⡀", "⢀", "⠠", "⠐", "⠈"];
const S11: &[&str] = &[
    "⢀⠀", "⡀⠀", "⠄⠀", "⢂⠀", "⡂⠀", "⠅⠀", "⢃⠀", "⡃⠀", "⠍⠀", "⢋⠀", "⡋⠀", "⠍⠁", "⢋⠁", "⡋⠁", "⠍⠉", "⠋⠉",
    "⠋⠉", "⠉⠙", "⠉⠙", "⠉⠩", "⠈⢙", "⠈⡙", "⢈⠩", "⡀⢙", "⠄⡙", "⢂⠩", "⡂⢘", "⠅⡘", "⢃⠨", "⡃⢐", "⠍⡐", "⢋⠠",
    "⡋⢀", "⠍⡁", "⢋⠁", "⡋⠁", "⠍⠉", "⠋⠉", "⠋⠉", "⠉⠙", "⠉⠙", "⠉⠩", "⠈⢙", "⠈⡙", "⠈⠩", "⠀⢙", "⠀⡙", "⠀⠩",
    "⠀⢘", "⠀⡘", "⠀⠨", "⠀⢐", "⠀⡐", "⠀⠠", "⠀⢀", "⠀⡀",
];
const S12: &[&str] = &["-", "\\", "|", "/"];
const S13: &[&str] = &["⠂", "-", "–", "—", "–", "-"];
const S14: &[&str] = &["┤", "┘", "┴", "└", "├", "┌", "┬", "┐"];
const S15: &[&str] = &[".  ", ".. ", "...", "   "];
const S16: &[&str] = &[".  ", ".. ", "...", " ..", "  .", "   "];
const S17: &[&str] = &["✶", "✸", "✹", "✺", "✹", "✷"];
const S18: &[&str] = &["+", "x", "*"];
const S19: &[&str] = &["_", "_", "_", "-", "`", "`", "'", "´", "-", "_", "_", "_"];
const S20: &[&str] = &["☱", "☲", "☴"];
const S21: &[&str] = &["▁", "▃", "▄", "▅", "▆", "▇", "▆", "▅", "▄", "▃"];
const S22: &[&str] = &["▏", "▎", "▍", "▌", "▋", "▊", "▉", "▊", "▋", "▌", "▍", "▎"];
const S23: &[&str] = &[" ", ".", "o", "O", "@", "*", " "];
const S24: &[&str] = &[".", "o", "O", "°", "O", "o", "."];
const S25: &[&str] = &["▓", "▒", "░"];
const S26: &[&str] = &["⠁", "⠂", "⠄", "⠂"];
const S27: &[&str] = &["▖", "▘", "▝", "▗"];
const S28: &[&str] = &["▌", "▀", "▐", "▄"];
const S29: &[&str] = &["◢", "◣", "◤", "◥"];
const S30: &[&str] = &["◜", "◠", "◝", "◞", "◡", "◟"];
const S31: &[&str] = &["◡", "⊙", "◠"];
const S32: &[&str] = &["◰", "◳", "◲", "◱"];
const S33: &[&str] = &["◴", "◷", "◶", "◵"];
const S34: &[&str] = &["◐", "◓", "◑", "◒"];
const S35: &[&str] = &["╫", "╪"];
const S36: &[&str] = &["⊶", "⊷"];
const S37: &[&str] = &["▫", "▪"];
const S38: &[&str] = &["□", "■"];
const S39: &[&str] = &["■", "□", "▪", "▫"];
const S40: &[&str] = &["▮", "▯"];
const S41: &[&str] = &["ဝ", "၀"];
const S42: &[&str] = &["⦾", "⦿"];
const S43: &[&str] = &["◍", "◌"];
const S44: &[&str] = &["◉", "◎"];
const S45: &[&str] = &["㊂", "㊀", "㊁"];
const S46: &[&str] = &["⧇", "⧆"];
const S47: &[&str] = &["☗", "☖"];
const S48: &[&str] = &["=", "*", "-"];
const S49: &[&str] = &["←", "↖", "↑", "↗", "→", "↘", "↓", "↙"];
const S50: &[&str] = &["▹▹▹▹▹", "▸▹▹▹▹", "▹▸▹▹▹", "▹▹▸▹▹", "▹▹▹▸▹", "▹▹▹▹▸"];
const S51: &[&str] = &[
    "[    ]", "[=   ]", "[==  ]", "[=== ]", "[ ===]", "[  ==]", "[   =]", "[    ]", "[   =]",
    "[  ==]", "[ ===]", "[====]", "[=== ]", "[==  ]", "[=   ]",
];
const S52: &[&str] = &[
    "( ●    )",
    "(  ●   )",
    "(   ●  )",
    "(    ● )",
    "(     ●)",
    "(    ● )",
    "(   ●  )",
    "(  ●   )",
    "( ●    )",
    "(●     )",
];
const S53: &[&str] = &[
    "▐⠂       ▌",
    "▐⠈       ▌",
    "▐ ⠂      ▌",
    "▐ ⠠      ▌",
    "▐  ⡀     ▌",
    "▐  ⠠     ▌",
    "▐   ⠂    ▌",
    "▐   ⠈    ▌",
    "▐    ⠂   ▌",
    "▐    ⠠   ▌",
    "▐     ⡀  ▌",
    "▐     ⠠  ▌",
    "▐      ⠂ ▌",
    "▐      ⠈ ▌",
    "▐       ⠂▌",
    "▐       ⠠▌",
    "▐       ⡀▌",
    "▐      ⠠ ▌",
    "▐      ⠂ ▌",
    "▐     ⠈  ▌",
    "▐     ⠂  ▌",
    "▐    ⠠   ▌",
    "▐    ⡀   ▌",
    "▐   ⠠    ▌",
    "▐   ⠂    ▌",
    "▐  ⠈     ▌",
    "▐  ⠂     ▌",
    "▐ ⠠      ▌",
    "▐ ⡀      ▌",
    "▐⠠       ▌",
];
const S54: &[&str] = &[
    "▐|\\____________▌",
    "▐_|\\___________▌",
    "▐__|\\__________▌",
    "▐___|\\_________▌",
    "▐____|\\________▌",
    "▐_____|\\_______▌",
    "▐______|\\______▌",
    "▐_______|\\_____▌",
    "▐________|\\____▌",
    "▐_________|\\___▌",
    "▐__________|\\__▌",
    "▐___________|\\_▌",
    "▐____________|\\▌",
    "▐____________/|▌",
    "▐___________/|_▌",
    "▐__________/|__▌",
    "▐_________/|___▌",
    "▐________/|____▌",
    "▐_______/|_____▌",
    "▐______/|______▌",
    "▐_____/|_______▌",
    "▐____/|________▌",
    "▐___/|_________▌",
    "▐__/|__________▌",
    "▐_/|___________▌",
    "▐/|____________▌",
];
const S55: &[&str] = &["d", "q", "p", "b"];
const S56: &[&str] = &[
    "،  ", "′  ", " ´ ", " ‾ ", "  ⸌", "  ⸊", "  |", "  ⁎", "  ⁕", " ෴ ", "  ⁓", "   ", "   ",
    "   ",
];
const S57: &[&str] = &["∙∙∙", "●∙∙", "∙●∙", "∙∙●", "∙∙∙"];
const S58: &[&str] = &["-", "=", "≡"];
const S59: &[&str] = &[
    "ρββββββ",
    "βρβββββ",
    "ββρββββ",
    "βββρβββ",
    "ββββρββ",
    "βββββρβ",
    "ββββββρ",
];
const S60: &[&str] = &[
    "▰▱▱▱▱▱▱",
    "▰▰▱▱▱▱▱",
    "▰▰▰▱▱▱▱",
    "▰▰▰▰▱▱▱",
    "▰▰▰▰▰▱▱",
    "▰▰▰▰▰▰▱",
    "▰▰▰▰▰▰▰",
    "▰▱▱▱▱▱▱",
];

pub(super) const BUILTIN_SPINNERS: &[SpinnerStyle] = &[
    SpinnerStyle {
        name: "dots",
        frames: S0,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots2",
        frames: S1,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots3",
        frames: S2,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots4",
        frames: S3,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots5",
        frames: S4,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots6",
        frames: S5,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots7",
        frames: S6,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots8",
        frames: S7,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots9",
        frames: S8,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots10",
        frames: S9,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "dots11",
        frames: S10,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "dots12",
        frames: S11,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "line",
        frames: S12,
        interval_ms: 130,
    },
    SpinnerStyle {
        name: "line2",
        frames: S13,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "pipe",
        frames: S14,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "simple_dots",
        frames: S15,
        interval_ms: 400,
    },
    SpinnerStyle {
        name: "simple_dots_scrolling",
        frames: S16,
        interval_ms: 200,
    },
    SpinnerStyle {
        name: "star",
        frames: S17,
        interval_ms: 70,
    },
    SpinnerStyle {
        name: "star2",
        frames: S18,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "flip",
        frames: S19,
        interval_ms: 70,
    },
    SpinnerStyle {
        name: "hamburger",
        frames: S20,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "grow_vertical",
        frames: S21,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "grow_horizontal",
        frames: S22,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "balloon",
        frames: S23,
        interval_ms: 140,
    },
    SpinnerStyle {
        name: "balloon2",
        frames: S24,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "noise",
        frames: S25,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "bounce",
        frames: S26,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "box_bounce",
        frames: S27,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "box_bounce2",
        frames: S28,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "triangle",
        frames: S29,
        interval_ms: 50,
    },
    SpinnerStyle {
        name: "arc",
        frames: S30,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "circle",
        frames: S31,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "square_corners",
        frames: S32,
        interval_ms: 180,
    },
    SpinnerStyle {
        name: "circle_quarters",
        frames: S33,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "circle_halves",
        frames: S34,
        interval_ms: 50,
    },
    SpinnerStyle {
        name: "squish",
        frames: S35,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "toggle",
        frames: S36,
        interval_ms: 250,
    },
    SpinnerStyle {
        name: "toggle2",
        frames: S37,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "toggle3",
        frames: S38,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "toggle4",
        frames: S39,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "toggle5",
        frames: S40,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "toggle6",
        frames: S41,
        interval_ms: 300,
    },
    SpinnerStyle {
        name: "toggle7",
        frames: S42,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "toggle8",
        frames: S43,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "toggle9",
        frames: S44,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "toggle10",
        frames: S45,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "toggle11",
        frames: S46,
        interval_ms: 50,
    },
    SpinnerStyle {
        name: "toggle12",
        frames: S47,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "toggle13",
        frames: S48,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "arrow",
        frames: S49,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "arrow3",
        frames: S50,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "bouncing_bar",
        frames: S51,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "bouncing_ball",
        frames: S52,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "pong",
        frames: S53,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "shark",
        frames: S54,
        interval_ms: 120,
    },
    SpinnerStyle {
        name: "dqpb",
        frames: S55,
        interval_ms: 100,
    },
    SpinnerStyle {
        name: "grenade",
        frames: S56,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "point",
        frames: S57,
        interval_ms: 125,
    },
    SpinnerStyle {
        name: "layer",
        frames: S58,
        interval_ms: 150,
    },
    SpinnerStyle {
        name: "beta_wave",
        frames: S59,
        interval_ms: 80,
    },
    SpinnerStyle {
        name: "aesthetic",
        frames: S60,
        interval_ms: 80,
    },
];

pub(super) fn builtin_spinner(name: &str) -> Option<SpinnerStyle> {
    BUILTIN_SPINNERS
        .iter()
        .copied()
        .find(|spinner| spinner.name == name)
}
