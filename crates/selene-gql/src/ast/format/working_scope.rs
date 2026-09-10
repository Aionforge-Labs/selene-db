//! Canonical formatting of bounded ISO working-scope hosts.

use super::fmt_ident;
use crate::{CatalogObjectReference, GraphExpression, IdentifierForm, WorkingScopeClause};
use std::fmt::{self, Write};

pub(super) fn prefixes(
    out: &mut String,
    clauses: &[WorkingScopeClause],
) -> Result<usize, fmt::Error> {
    let mut braces = 0;
    for clause in clauses {
        match clause {
            WorkingScopeClause::At { reference, .. } => {
                out.push_str("AT ");
                reference_text(out, reference, false)?;
                out.push(' ');
            }
            WorkingScopeClause::Use { expression, .. } => {
                out.push_str("USE ");
                match expression {
                    GraphExpression::Reference {
                        reference,
                        may_reference_binding,
                    } => reference_text(out, reference, !may_reference_binding)?,
                    GraphExpression::Variable { name, .. } => {
                        write!(out, "VARIABLE {}", fmt_ident(name.clone()))?
                    }
                    GraphExpression::Current { property, .. } => out.push_str(if *property {
                        "CURRENT_PROPERTY_GRAPH"
                    } else {
                        "CURRENT_GRAPH"
                    }),
                }
                out.push(' ');
            }
            WorkingScopeClause::Nested(_) => {
                out.push_str("{ ");
                braces += 1;
            }
        }
    }
    Ok(braces)
}

fn reference_text(
    out: &mut String,
    reference: &CatalogObjectReference,
    force_catalog: bool,
) -> fmt::Result {
    if !reference.absolute && force_catalog && reference.leaf().form == IdentifierForm::Regular {
        out.push_str("./");
    }
    for (index, segment) in reference.segments.iter().enumerate() {
        if reference.absolute || index > 0 {
            out.push('/');
        }
        match segment.form {
            IdentifierForm::Regular => out.push_str(&fmt_ident(segment.name.clone())),
            IdentifierForm::Delimited => {
                write!(out, "\"{}\"", segment.name.as_str().replace('"', "\"\""))?
            }
        }
    }
    Ok(())
}
