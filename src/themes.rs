use crate::daemon::AgentState;

pub struct Theme {
    pub css: &'static str,
    pub state_waiting: &'static str,
    pub state_working: &'static str,
    pub state_idle: &'static str,
}

impl Theme {
    pub fn state_color(&self, state: AgentState) -> &str {
        match state {
            AgentState::Waiting => self.state_waiting,
            AgentState::Responding => self.state_working,
            AgentState::Idle | AgentState::Unknown => self.state_idle,
        }
    }
}

const MOLOKAI: Theme = Theme {
    css: "\
@define-color bg rgba(30, 30, 30, 0.95);
@define-color accent #f92672;
@define-color separator rgba(255, 255, 255, 0.15);
@define-color text #ffffff;
@define-color text_dim #888888;
@define-color title #b5bd68;
@define-color key #f0c674;
@define-color selected #b5bd68;
",
    state_waiting: "#f92672",
    state_working: "#a6e22e",
    state_idle: "#888888",
};

const DEFAULT: Theme = Theme {
    css: "\
@define-color bg rgba(30, 30, 30, 0.95);
@define-color accent #4a9eff;
@define-color separator rgba(255, 255, 255, 0.15);
@define-color text #d4d4d4;
@define-color text_dim #808080;
@define-color title #73c991;
@define-color key #e5c07b;
@define-color selected #73c991;
",
    state_waiting: "#e5c07b",
    state_working: "#73c991",
    state_idle: "#808080",
};

pub fn get(name: &str) -> &'static Theme {
    match name {
        "molokai" => &MOLOKAI,
        _ => &DEFAULT,
    }
}
