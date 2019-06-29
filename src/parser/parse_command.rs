use crate::errors::ShellError;
use crate::parser::registry::{CommandConfig, CommandRegistry, NamedType};
use crate::parser::{baseline_parse_tokens, CallNode, Spanned};
use crate::parser::{
    hir::{self, NamedArguments},
    Flag, RawToken, TokenNode,
};
use crate::Text;
use log::trace;

pub fn parse_command(
    config: &CommandConfig,
    registry: &dyn CommandRegistry,
    call: &Spanned<CallNode>,
    source: &Text,
) -> Result<hir::Call, ShellError> {
    let Spanned { item: call, .. } = call;

    trace!("Processing {:?}", config);

    let head = parse_command_head(call.head())?;

    let children: Option<Vec<TokenNode>> = call.children().as_ref().map(|nodes| {
        nodes
            .iter()
            .cloned()
            .filter(|node| match node {
                TokenNode::Whitespace(_) => false,
                _ => true,
            })
            .collect()
    });

    match parse_command_tail(&config, registry, children, source)? {
        None => Ok(hir::Call::new(Box::new(head), None, None)),
        Some((positional, named)) => Ok(hir::Call::new(Box::new(head), positional, named)),
    }
}

fn parse_command_head(head: &TokenNode) -> Result<hir::Expression, ShellError> {
    match head {
        TokenNode::Token(
            spanned @ Spanned {
                item: RawToken::Bare,
                ..
            },
        ) => Ok(spanned.map(|_| hir::RawExpression::Literal(hir::Literal::Bare))),

        TokenNode::Token(Spanned {
            item: RawToken::String(inner_span),
            span,
        }) => Ok(Spanned::from_item(
            hir::RawExpression::Literal(hir::Literal::String(*inner_span)),
            *span,
        )),

        other => Err(ShellError::unexpected(&format!(
            "command head -> {:?}",
            other
        ))),
    }
}

fn parse_command_tail(
    config: &CommandConfig,
    registry: &dyn CommandRegistry,
    tail: Option<Vec<TokenNode>>,
    source: &Text,
) -> Result<Option<(Option<Vec<hir::Expression>>, Option<NamedArguments>)>, ShellError> {
    let tail = &mut match &tail {
        None => return Ok(None),
        Some(tail) => hir::TokensIterator::new(tail),
    };

    let mut named = NamedArguments::new();

    trace_remaining("nodes", tail.clone(), source);

    for (name, kind) in config.named() {
        trace!("looking for {} : {:?}", name, kind);

        match kind {
            NamedType::Switch => {
                let flag = extract_switch(name, tail, source);

                named.insert_switch(name, flag);
            }
            NamedType::Mandatory(kind) => match extract_mandatory(name, tail, source) {
                Err(err) => return Err(err), // produce a correct diagnostic
                Ok((pos, _flag)) => {
                    tail.move_to(pos);
                    let expr = hir::baseline_parse_next_expr(
                        tail,
                        registry,
                        source,
                        kind.to_coerce_hint(),
                    )?;

                    tail.restart();
                    named.insert_mandatory(name, expr);
                }
            },
            NamedType::Optional(kind) => match extract_optional(name, tail, source) {
                Err(err) => return Err(err), // produce a correct diagnostic
                Ok(Some((pos, _flag))) => {
                    tail.move_to(pos);
                    let expr = hir::baseline_parse_next_expr(
                        tail,
                        registry,
                        source,
                        kind.to_coerce_hint(),
                    )?;

                    tail.restart();
                    named.insert_optional(name, Some(expr));
                }

                Ok(None) => {
                    tail.restart();
                    named.insert_optional(name, None);
                }
            },
        };
    }

    trace_remaining("after named", tail.clone(), source);

    let mut positional = vec![];
    let mandatory = config.mandatory_positional();

    for arg in mandatory {
        trace!("Processing mandatory {:?}", arg);

        if tail.len() == 0 {
            return Err(ShellError::unimplemented("Missing mandatory argument"));
        }

        let result = hir::baseline_parse_next_expr(tail, registry, source, arg.to_coerce_hint())?;

        positional.push(result);
    }

    trace_remaining("after mandatory", tail.clone(), source);

    let optional = config.optional_positional();

    for arg in optional {
        if tail.len() == 0 {
            break;
        }

        let result = hir::baseline_parse_next_expr(tail, registry, source, arg.to_coerce_hint())?;

        positional.push(result);
    }

    trace_remaining("after optional", tail.clone(), source);

    // TODO: Only do this if rest params are specified
    let remainder = baseline_parse_tokens(tail, registry, source)?;
    positional.extend(remainder);

    trace_remaining("after rest", tail.clone(), source);

    trace!("Constructed positional={:?} named={:?}", positional, named);

    let positional = match positional {
        positional if positional.len() == 0 => None,
        positional => Some(positional),
    };

    let named = match named {
        named if named.named.is_empty() => None,
        named => Some(named),
    };

    trace!("Normalized positional={:?} named={:?}", positional, named);

    Ok(Some((positional, named)))
}

fn extract_switch(name: &str, tokens: &mut hir::TokensIterator<'_>, source: &Text) -> Option<Flag> {
    tokens
        .extract(|t| t.as_flag(name, source))
        .map(|(_pos, flag)| flag.item)
}

fn extract_mandatory(
    name: &str,
    tokens: &mut hir::TokensIterator<'a>,
    source: &Text,
) -> Result<(usize, Flag), ShellError> {
    let flag = tokens.extract(|t| t.as_flag(name, source));

    match flag {
        None => Err(ShellError::unimplemented(
            "Better error: mandatory flags must be present",
        )),
        Some((pos, flag)) => {
            if tokens.len() <= pos {
                return Err(ShellError::unimplemented(
                    "Better errors: mandatory flags must be followed by values",
                ));
            }

            tokens.remove(pos);

            Ok((pos, *flag))
        }
    }
}

fn extract_optional(
    name: &str,
    tokens: &mut hir::TokensIterator<'a>,
    source: &Text,
) -> Result<(Option<(usize, Flag)>), ShellError> {
    let flag = tokens.extract(|t| t.as_flag(name, source));

    match flag {
        None => Ok(None),
        Some((pos, flag)) => {
            if tokens.len() <= pos {
                return Err(ShellError::unimplemented(
                    "Better errors: optional flags must be followed by values",
                ));
            }

            tokens.remove(pos);

            Ok(Some((pos, *flag)))
        }
    }
}

pub fn trace_remaining(desc: &'static str, tail: hir::TokensIterator<'a>, source: &Text) {
    trace!(
        "{} = {:?}",
        desc,
        itertools::join(
            tail.debug_remaining()
                .iter()
                .map(|i| format!("%{:?}%", i.debug(source))),
            " "
        )
    );
}