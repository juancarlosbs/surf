use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapOptions {
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub diff: bool,
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default = "default_max_elements")]
    pub max_elements: usize,
    #[serde(default = "default_max_text_chars")]
    pub max_text_chars: usize,
}

impl Default for SnapOptions {
    fn default() -> Self {
        Self {
            all: false,
            diff: false,
            filter: None,
            max_elements: default_max_elements(),
            max_text_chars: default_max_text_chars(),
        }
    }
}

pub fn default_max_elements() -> usize {
    150
}

pub fn default_max_text_chars() -> usize {
    4000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    Stop,
    Open {
        url: String,
        #[serde(default)]
        snap: SnapOptions,
    },
    Snap {
        #[serde(default)]
        opts: SnapOptions,
    },
    Click {
        ref_id: u32,
        #[serde(default = "default_true")]
        diff: bool,
    },
    ClickLabel {
        label: String,
        #[serde(default = "default_true")]
        diff: bool,
    },
    Fill {
        ref_id: u32,
        text: String,
        #[serde(default = "default_true")]
        diff: bool,
    },
    FillLabel {
        label: String,
        text: String,
        #[serde(default = "default_true")]
        diff: bool,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
    },
    Scroll {
        direction: ScrollDirection,
        amount: i32,
    },
    Wait {
        target: WaitTarget,
    },
    Eval {
        expr: String,
    },
    Screenshot {
        path: Option<String>,
    },
    Tabs,
    TabSwitch {
        index: usize,
    },
    Login {
        url: Option<String>,
    },
    TabNew {
        url: Option<String>,
    },
    TabClose {
        index: Option<usize>,
    },
    Batch {
        steps: Vec<Request>,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    Up,
    Down,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitTarget {
    Ms { ms: u64 },
    Idle { timeout_ms: u64 },
    Text { text: String, timeout_ms: u64 },
    Label { label: String, timeout_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchConfig {
    pub port: u16,
    pub headless: bool,
    pub attach_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(flatten)]
    pub req: Request,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub name: String,
    pub us: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<Phase>,
}

impl Response {
    pub fn ok(output: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            ok: true,
            output: Some(output.into()),
            error: None,
            elapsed_ms: Some(elapsed_ms),
            phases: Vec::new(),
        }
    }

    pub fn err(error: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            ok: false,
            output: None,
            error: Some(error.into()),
            elapsed_ms: Some(elapsed_ms),
            phases: Vec::new(),
        }
    }
}
