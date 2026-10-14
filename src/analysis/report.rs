use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    Macro,
    EnumConstant,
    EnumType,
    Config,
    NotFound,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextInfo {
    pub tree: String,
    pub objtree: Option<String>,
    pub release: String,
    pub commit: String,
    pub arch: String,
    pub compiler: String,
    pub predefines: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compiler_headers: Vec<HeaderDir>,
    pub mode: String,
    pub what_if: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeaderDir {
    pub used: String,
    pub recorded: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Site {
    pub path: String,
    pub line: u32,
    pub text: String,
    pub origin: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Atom {
    pub text: String,
    pub value: String,
    pub site: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Condition {
    pub path: String,
    pub line: u32,
    pub directive: String,
    pub taken: bool,
    pub evaluated: bool,
    pub atoms: Vec<Atom>,
    #[serde(default)]
    pub prior: Vec<Atom>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Value {
    pub display: String,
    pub ty: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Replaced,
    Overridden,
    Undefined,
    ConditionFalse,
    NotReached,
    Elsewhere,
    OtherArch,
    Host,
    TuLocal,
    NotIncluded,
    Shadowed,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Replaced => "what-if replaced",
            Status::Overridden => "overridden",
            Status::Undefined => "undefined later",
            Status::ConditionFalse => "condition false",
            Status::NotReached => "not reached",
            Status::Elsewhere => "other context",
            Status::OtherArch => "other arch",
            Status::Host => "host/userspace",
            Status::TuLocal => "TU-local",
            Status::NotIncluded => "not included",
            Status::Shadowed => "shadowed",
        }
    }

    pub fn order(self) -> u8 {
        match self {
            Status::Active => 0,
            Status::Replaced => 1,
            Status::Overridden => 2,
            Status::Undefined => 3,
            Status::ConditionFalse => 4,
            Status::NotReached => 5,
            Status::Elsewhere => 6,
            Status::Shadowed => 7,
            Status::NotIncluded => 8,
            Status::TuLocal => 9,
            Status::OtherArch => 10,
            Status::Host => 11,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub path: String,
    pub line: u32,
    pub text: String,
    pub status: Status,
    pub detail: String,
    pub conditions: Vec<Condition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Arg {
    pub param: String,
    pub raw: String,
    pub expanded: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refined: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExpNode {
    pub name: String,
    pub call: String,
    pub site: Option<Site>,
    pub why: Vec<Condition>,
    pub alternatives: Vec<Candidate>,
    pub args: Vec<Arg>,
    pub substituted: String,
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refined: Option<String>,
    pub value: Option<Value>,
    pub children: Vec<ExpNode>,
    pub note: Option<String>,
    pub repeat_of: Option<String>,
    pub config: Option<String>,
    pub kconfig: Option<KconfigInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<CTarget>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CTarget {
    pub kind: String,
    pub lines: Vec<String>,
    pub hidden: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Variant {
    pub site: Option<Site>,
    pub expansion: Option<String>,
    pub value: Option<Value>,
    pub tu_count: usize,
    pub sample: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Census {
    pub tu_total: usize,
    pub tu_examined: usize,
    pub source: String,
    pub variants: Vec<Variant>,
    pub not_built: Vec<String>,
    pub seconds: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnumMember {
    pub name: String,
    pub line: u32,
    pub init: Option<String>,
    pub active: bool,
    pub value: Option<Value>,
    pub expansion: Option<String>,
    pub excluded_by: Option<Condition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnumInfo {
    pub tag: Option<String>,
    pub site: Site,
    pub members: Vec<EnumMember>,
    pub focus: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KconfigDefault {
    pub value: String,
    pub condition: Option<String>,
    pub condition_value: Option<String>,
    pub chosen: bool,
    pub site: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KconfigInfo {
    pub symbol: String,
    pub kind: String,
    pub value: String,
    pub sites: Vec<String>,
    pub prompt: Option<String>,
    pub visible: Option<String>,
    pub depends: Option<String>,
    pub depends_value: Option<String>,
    pub defaults: Vec<KconfigDefault>,
    pub selected_by: Vec<(String, String)>,
    pub implied_by: Vec<(String, String)>,
    pub choice: Option<Vec<(String, String)>>,
    pub range: Option<String>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub query: String,
    pub kind: SubjectKind,
    pub context: ContextInfo,
    pub active: Option<Site>,
    pub active_conditions: Vec<Condition>,
    pub expansion: Option<ExpNode>,
    pub value: Option<Value>,
    pub candidates: Vec<Candidate>,
    pub census: Option<Census>,
    pub enum_info: Option<EnumInfo>,
    pub kconfig: Option<KconfigInfo>,
    pub tu_dependent: Option<String>,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}
