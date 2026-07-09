//! The orchestration pipeline (master plan §2.1–§2.2).
//!
//! Runs the five roles in sequence, threading each stage's output into the next.
//! The Auditor stage emits AINL under the GBNF grammar and its output is
//! validated in-process with `ainl_core::parse` — the "syntax enforcer" is real,
//! not aspirational. The final product is a copy-pasteable, constraint-based
//! prompt (§2.2) built around the validated AINL schema.

use crate::backend::{Backend, GenRequest};
use crate::context::ContextProvider;
use crate::role::Role;
use ainl_core::Result;

pub struct StageOutput {
    pub role: Role,
    pub model: String,
    pub output: String,
}

pub struct AuditReport {
    pub input: String,
    pub backend: String,
    pub stages: Vec<StageOutput>,
    /// The audited AINL schema (Auditor stage output).
    pub ainl: String,
    pub ainl_valid: bool,
    pub ainl_error: Option<String>,
    /// The Generalist's friendly summary.
    pub summary: String,
    /// The final copy-pasteable, constraint-based prompt for a frontier model.
    pub context_prompt: String,
}

pub struct Auditor<'b> {
    backend: &'b dyn Backend,
    providers: Vec<Box<dyn ContextProvider>>,
}

impl<'b> Auditor<'b> {
    pub fn new(backend: &'b dyn Backend) -> Auditor<'b> {
        Auditor { backend, providers: Vec::new() }
    }

    pub fn with_context(mut self, provider: Box<dyn ContextProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    fn run_role(&self, role: Role, request: &str, prompt: &str, grammar: Option<&str>) -> Result<String> {
        let req = GenRequest { role, system: role.system_prompt(), request, prompt, grammar };
        Ok(self.backend.generate(&req)?.trim().to_string())
    }

    /// Gather external context (§2.3) for the request, best-effort: a provider
    /// that errors is skipped rather than failing the whole run.
    fn gather_context(&self, input: &str) -> String {
        let mut ctx = String::new();
        for p in &self.providers {
            if let Ok(text) = p.fetch(input) {
                if !text.trim().is_empty() {
                    ctx.push_str(&format!("[context from {}]\n{}\n", p.name(), text));
                }
            }
        }
        ctx
    }

    pub fn run(&self, input: &str) -> Result<AuditReport> {
        let mut stages = Vec::new();
        let mut record = |role: Role, output: String| {
            stages.push(StageOutput { role, model: role.model().to_string(), output });
        };

        // 1. Orchestrator — intent + route.
        let orchestration = self.run_role(Role::Orchestrator, input, input, None)?;
        record(Role::Orchestrator, orchestration.clone());

        // 2. Planner — logic & edge cases (enriched with external context).
        let context = self.gather_context(input);
        let planner_prompt = format!(
            "Request:\n{input}\n\nOrchestrator analysis:\n{orchestration}\n{}",
            if context.is_empty() { String::new() } else { format!("\nRelevant context:\n{context}") }
        );
        let plan = self.run_role(Role::Planner, input, &planner_prompt, None)?;
        record(Role::Planner, plan.clone());

        // 3. Auditor — grammar-constrained AINL, then validate it parses.
        let auditor_prompt = format!("Plan:\n{plan}\n\nEmit the AINL schema.");
        let ainl = self.run_role(Role::Auditor, input, &auditor_prompt, Some(ainl_core::GBNF))?;
        record(Role::Auditor, ainl.clone());
        let (ainl_valid, ainl_error) = match ainl_core::parse(&ainl) {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };

        // 4. Code Engine — final dense AINL from the schema.
        let code = self.run_role(Role::CodeEngine, input, &format!("AINL schema:\n{ainl}"), Some(ainl_core::GBNF))?;
        record(Role::CodeEngine, code.clone());

        // 5. Generalist — friendly summary.
        let summary = self.run_role(
            Role::Generalist,
            input,
            &format!("Request: {input}\nResult (AINL):\n{code}"),
            None,
        )?;
        record(Role::Generalist, summary.clone());

        let context_prompt = build_context_prompt(input, &plan, &ainl, ainl_valid);

        Ok(AuditReport {
            input: input.to_string(),
            backend: self.backend.describe(),
            stages,
            ainl,
            ainl_valid,
            ainl_error,
            summary,
            context_prompt,
        })
    }
}

/// Assemble the audited, constraint-based prompt a user pastes into any frontier
/// model for one-shot generation (§2.2).
fn build_context_prompt(input: &str, plan: &str, ainl: &str, valid: bool) -> String {
    format!(
        "# Audited request (compiled by the AINL Prompt Auditor)\n\n\
         **Original:** {input}\n\n\
         ## Plan\n{plan}\n\n\
         ## AINL schema{}\n\
         The following is a validated AINL program (S-expression, `(op arg...)`). \
         Implement exactly this behavior.\n\n\
         ```ainl\n{ainl}\n```\n\n\
         ## Task for the target model\n\
         Generate a correct, complete implementation that satisfies the AINL schema \
         above. Do not ask clarifying questions — the schema is the specification.\n",
        if valid { " ✓ grammar-valid" } else { " ⚠ did not parse" }
    )
}
