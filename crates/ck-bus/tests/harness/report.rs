//! Row reports, checked against the acceptance ladder of `docs/specs/ck-bus-module.md`
//! (revision 2): the rows, the skip names each row may record, and the side that served
//! each row.
//!
//! Every row file compiles this whole module and uses only part of it.
#![allow(dead_code)]

use std::collections::BTreeSet;

use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Row {
    ModuleDeclaration,
    SupervisedServer,
    SupervisedServerArgv,
    GrantGeneration,
    SignerShape,
    VaultAuthorization,
    UserJwt,
    AccountIdentity,
    InstallBootstrap,
    CredentialDelivery,
    Census,
    GrantConformance,
    Revocation,
    SpawnStream,
    SpawnReconcile,
    ModuleHealth,
    Sentinel,
    DeadLetter,
    Membership,
    SignerOutage,
    FedAccount,
    Leaf,
    LeafLink,
    FedSeal,
    FedOpen,
    FedSequence,
    FedSplit,
    FedRemoval,
}

/// Which side answered a row's Claustrum and Callosum routes. A row served by two sides
/// records both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ServedBy {
    /// No Claustrum or Callosum route was opened.
    None,
    /// The shape stubs: recorded reply shapes, no signatures.
    HarnessStub,
    /// The harness signer: real signatures from throwaway fixture keys, no authority.
    HarnessSigner,
    /// A real claustrum found through `CK_CLAUSTRUM_BIN`.
    ClaustrumBinary,
}

impl ServedBy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::HarnessStub => "harness-stub",
            Self::HarnessSigner => "harness-signer",
            Self::ClaustrumBinary => "claustrum-binary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Skipped {
        gate: &'static str,
        observation: String,
    },
}

#[derive(Debug, Clone)]
pub struct RowReport {
    row: Row,
    outcome: Outcome,
    served_by: BTreeSet<ServedBy>,
    reached_operations: BTreeSet<String>,
    asserts_authorization: bool,
}

impl RowReport {
    pub fn passed(row: Row) -> Self {
        Self::new(row, Outcome::Passed)
    }

    pub fn skipped(row: Row, gate: &'static str, observation: impl Into<String>) -> Self {
        Self::new(
            row,
            Outcome::Skipped {
                gate,
                observation: observation.into(),
            },
        )
    }

    fn new(row: Row, outcome: Outcome) -> Self {
        Self {
            row,
            outcome,
            served_by: BTreeSet::new(),
            reached_operations: BTreeSet::new(),
            asserts_authorization: false,
        }
    }

    /// Records a side that served this row.
    pub fn served_by(mut self, side: ServedBy) -> Self {
        self.served_by.insert(side);
        self
    }

    /// Records that the row reached `operation` on its serving side; `validate` checks it
    /// against that side's advertised vocabulary.
    pub fn reached(mut self, operation: impl Into<String>) -> Self {
        self.reached_operations.insert(operation.into());
        self
    }

    /// Shorthand for a row served by the shape stubs that reached `operation`.
    pub fn served_by_harness_stub(self, operation: impl Into<String>) -> Self {
        self.served_by(ServedBy::HarnessStub).reached(operation)
    }

    /// Marks the row as asserting vault authorization, which only a real claustrum can
    /// prove.
    pub fn asserts_authorization(mut self) -> Self {
        self.asserts_authorization = true;
        self
    }

    /// Applies the ladder's gating rules. `advertised` is the vocabulary of the side that
    /// served the reached operations.
    pub fn validate(&self, advertised: &BTreeSet<String>) -> Result<(), String> {
        if let Outcome::Skipped { gate, observation } = &self.outcome {
            if gate.trim().is_empty() || observation.trim().is_empty() {
                return Err("a skipped row must name its gate and observed condition".to_string());
            }
            if !allowed_gates(self.row).contains(gate) && !UNIVERSAL_GATES.contains(gate) {
                return Err(format!("row {:?} may not record gate {gate}", self.row));
            }
        }
        if self.served_by.is_empty() {
            return Err(format!("row {:?} omits its served-by", self.row));
        }
        if self.served_by.contains(&ServedBy::None) && self.served_by.len() > 1 {
            return Err(format!(
                "row {:?} records served-by none together with a serving side",
                self.row
            ));
        }
        if self.asserts_authorization
            && self.served_by != BTreeSet::from([ServedBy::ClaustrumBinary])
        {
            return Err(format!(
                "row {:?} asserts authorization but was served by {}; only claustrum-binary \
                 proves vault authority",
                self.row,
                self.served_by_label()
            ));
        }
        if !self.reached_operations.is_empty() && self.served_by.contains(&ServedBy::None) {
            return Err(format!(
                "row {:?} reached serving-side operations but records served-by none",
                self.row
            ));
        }
        for operation in &self.reached_operations {
            if !advertised.contains(operation) {
                return Err(format!(
                    "row {:?} reached unadvertised operation {operation}",
                    self.row
                ));
            }
        }
        Ok(())
    }

    pub fn served_by_label(&self) -> String {
        self.served_by
            .iter()
            .map(|side| side.as_str())
            .collect::<Vec<_>>()
            .join(" + ")
    }

    /// Validates the report and prints it as one JSON line on stderr. A skip is printed
    /// with a loud `SKIP` banner as well, because a skipped row is never a pass.
    pub fn emit(&self, advertised: &BTreeSet<String>) {
        if let Err(error) = self.validate(advertised) {
            panic!("row report refused by the harness: {error}");
        }
        let (outcome, gate, observation) = match &self.outcome {
            Outcome::Passed => ("passed", None, None),
            Outcome::Skipped { gate, observation } => {
                ("skipped", Some(*gate), Some(observation.as_str()))
            }
        };
        if let (Some(gate), Some(observation)) = (gate, observation) {
            eprintln!(
                "SKIP {:?} (served-by: {}): {gate}: {observation}",
                self.row,
                self.served_by_label()
            );
        }
        eprintln!(
            "{}",
            json!({
                "row": format!("{:?}", self.row),
                "served_by": self.served_by_label(),
                "outcome": outcome,
                "gate": gate,
                "observation": observation,
                "reached": self.reached_operations,
            })
        );
    }
}

/// Recordable by any row without listing (the ladder's universal conditions).
const UNIVERSAL_GATES: &[&str] = &[
    "naming-constructor-absent",
    "health-class-carrier-unpinned",
    "stub-reply-shape-unrecorded",
    "nats-server-absent",
    "nats-server-too-old",
];

/// Each row's Gates cell in the acceptance ladder.
fn allowed_gates(row: Row) -> &'static [&'static str] {
    match row {
        Row::ModuleDeclaration
        | Row::GrantGeneration
        | Row::UserJwt
        | Row::AccountIdentity
        | Row::InstallBootstrap
        | Row::Census
        | Row::Revocation
        | Row::ModuleHealth
        | Row::Sentinel
        | Row::DeadLetter
        | Row::SignerOutage
        | Row::Leaf => &[],
        Row::SupervisedServer | Row::SupervisedServerArgv => &["a1-signal-unix-only"],
        Row::SignerShape | Row::VaultAuthorization => &["claustrum-binary-absent"],
        Row::CredentialDelivery | Row::SpawnStream | Row::SpawnReconcile => {
            &["spawn-stream-unlanded"]
        }
        Row::GrantConformance => &["prefrontal-seat-unnamed"],
        Row::Membership => &["membership-contract-unpinned"],
        Row::FedAccount | Row::FedSplit => {
            &["fed-foundation-amendment-unlanded", "nats-federation-rig"]
        }
        Row::LeafLink => &["leaf-credential-ceremony-unlanded", "nats-federation-rig"],
        Row::FedSeal | Row::FedRemoval => &["fed-foundation-amendment-unlanded"],
        Row::FedOpen | Row::FedSequence => &[
            "fed-foundation-amendment-unlanded",
            "kemkey-open-unlanded",
            "claustrum-binary-absent",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::{Row, RowReport, ServedBy};
    use std::collections::BTreeSet;

    fn signer_ops() -> BTreeSet<String> {
        ["credential.sign", "credential.public_key"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_row_without_served_by_is_refused() {
        let error = RowReport::passed(Row::UserJwt)
            .validate(&signer_ops())
            .expect_err("a report with no serving side must be refused");
        assert!(error.contains("omits its served-by"), "{error}");
    }

    #[test]
    fn authorization_served_by_anything_but_the_real_binary_is_refused() {
        let error = RowReport::passed(Row::VaultAuthorization)
            .served_by(ServedBy::HarnessSigner)
            .asserts_authorization()
            .validate(&signer_ops())
            .expect_err("a harness-signer authorization pass must be refused");
        assert!(error.contains("only claustrum-binary"), "{error}");
        RowReport::passed(Row::VaultAuthorization)
            .served_by(ServedBy::ClaustrumBinary)
            .asserts_authorization()
            .validate(&signer_ops())
            .expect("an authorization pass served by the real binary is valid");
    }

    #[test]
    fn signer_row_reaching_an_unadvertised_op_is_refused() {
        let error = RowReport::passed(Row::SignerShape)
            .served_by(ServedBy::HarnessSigner)
            .reached("credential.get")
            .validate(&signer_ops())
            .expect_err("an op outside the signer's vocabulary must be refused");
        assert!(error.contains("unadvertised operation"), "{error}");
    }

    #[test]
    fn deleted_r1_gates_are_no_longer_recordable() {
        for gate in ["ckcred-mint-unlanded", "ckcred-delete-unlanded"] {
            let error = RowReport::skipped(Row::InstallBootstrap, gate, "observed")
                .served_by(ServedBy::HarnessSigner)
                .validate(&signer_ops())
                .expect_err("a gate deleted in revision 2 must be refused");
            assert!(error.contains("may not record gate"), "{error}");
        }
    }
}
