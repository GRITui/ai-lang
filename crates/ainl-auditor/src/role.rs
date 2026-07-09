//! The five specialized roles of the orchestration swarm (master plan §2.1).
//!
//! Each role maps to a specific sub-10B local model and a specialized system
//! prompt. Model tags follow the conventional `name:size` form used by local
//! runners (Ollama / llama.cpp).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Ingests the request, analyzes intent, decides the execution path.
    Orchestrator,
    /// Maps logic, edge cases, and step-by-step reasoning for complex work.
    Planner,
    /// Grammar-constrained: turns the plan into a strict, valid AINL schema.
    Auditor,
    /// Consumes the schema to generate the final dense AINL / logic.
    CodeEngine,
    /// Summarizes technical output into a friendly, human-readable result.
    Generalist,
}

impl Role {
    /// Execution order of the pipeline.
    pub const PIPELINE: [Role; 5] = [
        Role::Orchestrator,
        Role::Planner,
        Role::Auditor,
        Role::CodeEngine,
        Role::Generalist,
    ];

    /// The local model assigned to this role in the master plan.
    pub fn model(self) -> &'static str {
        match self {
            Role::Orchestrator => "qwen3.5:9b",
            Role::Planner => "deepseek-r1-distill-qwen:7b",
            Role::Auditor => "phi-4-mini:3.8b",
            Role::CodeEngine => "granite4.1:8b",
            Role::Generalist => "llama3.1:8b-instruct",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Role::Orchestrator => "Orchestrator",
            Role::Planner => "Planner",
            Role::Auditor => "Auditor / Syntax Enforcer",
            Role::CodeEngine => "Code Engine",
            Role::Generalist => "Generalist",
        }
    }

    /// The role's specialized system prompt.
    pub fn system_prompt(self) -> &'static str {
        match self {
            Role::Orchestrator => {
                "You are the Orchestrator. Read the user's request, state its core \
intent in one line, and decide the execution path: `code` (needs logic/architecture) or \
`text` (informational). Respond concisely."
            }
            Role::Planner => {
                "You are the Planner. Given the intent, lay out the logic as numbered \
steps, list edge cases, and note required inputs/outputs. Be precise and exhaustive."
            }
            Role::Auditor => {
                "You are the Auditor / Syntax Enforcer. Emit ONLY a valid AINL program \
that encodes the plan. AINL is an S-expression language: (op arg...). Use def/fn/if/let/while and \
builtins (+ - * / = < > print list len first rest cons). Output nothing but the AINL."
            }
            Role::CodeEngine => {
                "You are the Code Engine. Consume the AINL schema and produce the \
final, dense AINL implementation. Keep it minimal and correct."
            }
            Role::Generalist => {
                "You are the Generalist. Summarize the technical result in a friendly, \
plain-language paragraph for a non-expert. Do not include code."
            }
        }
    }
}
