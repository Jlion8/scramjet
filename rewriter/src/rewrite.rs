use core::str;
use std::str::from_utf8;

use oxc::{
	allocator::Allocator,
	ast::{
		ast::{
			AssignmentExpression, AssignmentTarget, CallExpression, DebuggerStatement,
			ExportAllDeclaration, ExportNamedDeclaration, Expression, ForInStatement,
			ForOfStatement, FunctionBody, IdentifierReference, ImportDeclaration, ImportExpression,
			MemberExpression, MetaProperty, NewExpression, ObjectExpression, ObjectPropertyKind,
			ReturnStatement, ThisExpression, UnaryExpression, UpdateExpression, Statement
		},
		visit::walk,
		Visit,
	},
	diagnostics::OxcDiagnostic,
	parser::{ParseOptions, Parser},
	span::{Atom, GetSpan, SourceType, Span},
	syntax::operator::{AssignmentOperator, UnaryOperator},
};
use url::Url;

use crate::error::{Result, RewriterError};

#[derive(Debug)]
enum JsChange {
	GenericChange {
		span: Span,
		text: String,
	},
	SourceTag {
		tagstart: u32,
	},
	Assignment {
		name: String,
		entirespan: Span,
		rhsspan: Span,
		op: AssignmentOperator,
	},
}

pub type EncodeFn = Box<dyn Fn(String) -> String>;
struct Rewriter {
	jschanges: Vec<JsChange>,
	base: Url,
	config: Config,
}

pub struct Config {
	pub prefix: String,

	pub wrapfn: String,
	pub wrapthisfn: String,
	pub importfn: String,
	pub rewritefn: String,
	pub setrealmfn: String,
	pub metafn: String,
	pub pushsourcemapfn: String,

	pub encode: EncodeFn,
	pub capture_errors: bool,
	pub scramitize: bool,
	pub do_sourcemaps: bool,
	pub strict_rewrites: bool,
}

impl Rewriter {
	fn rewrite_url(&mut self, url: String) -> String {
		let url = self.base.join(&url).unwrap();

		let urlencoded = (self.config.encode)(url.to_string());

		format!("\"{}{}?type=module\"", self.config.prefix, urlencoded)
	}

	fn rewrite_ident(&mut self, name: &Atom, span: Span) {
		if UNSAFE_GLOBALS.contains(&name.to_string().as_str()) {
			self.jschanges.push(JsChange::GenericChange {
				span,
				text: format!("({}({}))", self.config.wrapfn, name),
			});
		}
	}

	fn walk_member_expression(&mut self, it: &Expression) -> bool {
		if match it {
			Expression::Identifier(s) => {
				self.rewrite_ident(&s.name, s.span);
				true
			}
			Expression::StaticMemberExpression(s) => self.walk_member_expression(&s.object),
			Expression::ComputedMemberExpression(s) => self.walk_member_expression(&s.object),
			_ => false,
		} {
			return true;
		}
		// TODO: WE SHOULD PROBABLY WALK THE REST OF THE TREE
		// walk::walk_expression(self, it);
		false
	}
}

impl<'a> Visit<'a> for Rewriter {
	fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
		// if self.config.capture_errors {
		// 	self.jschanges.push(JsChange::GenericChange {
		// 		span: it.span,
		// 		text: format!(
		// 			"{}({}, typeof arguments != 'undefined' && arguments)",
		// 			self.config.wrapfn, it.name
		// 		),
		// 	});
		// } else {
		// if UNSAFE_GLOBALS.contains(&it.name.to_string().as_str()) {
		// 	self.jschanges.push(JsChange::GenericChange {
		// 		span: it.span,
		// 		text: format!("{}({})", self.config.wrapfn, it.name),
		// 	});
		// }
		// }
	}

	// fn visit_program(&mut self, it: &oxc_ast::ast::Program<'a>) {
	//     it.directives[0].
	// }
	// we need to rewrite `new Something` to `new (wrapfn(Something))` instead of `new wrapfn(Something)`, that's why there's weird extra code here
	// fn visit_new_expression(&mut self, it: &oxc_ast::ast::NewExpression<'a>) {
	// 	// self.walk_member_expression(&it.callee);
	// 	walk::walk_arguments(self, &it.arguments);
	// }
	fn visit_member_expression(&mut self, it: &MemberExpression<'a>) {
		match it {
			MemberExpression::StaticMemberExpression(s) => {
				if s.property.name == "postMessage" {
					self.jschanges.push(JsChange::GenericChange {
						span: s.property.span,
						// an empty object will let us safely reconstruct the realm later
						text: format!("{}({{}}).{}", self.config.setrealmfn, s.property.name),
					});

					return; // unwise to walk the rest of the tree
				}

				if self.config.scramitize
					&& !matches!(s.object, Expression::MetaProperty(_))
					&& !matches!(s.object, Expression::Super(_))
				{
					let span = s.object.span();
					self.jschanges.push(JsChange::GenericChange {
						span: Span::new(span.start, span.start),
						text: " $scramitize(".to_string(),
					});
					self.jschanges.push(JsChange::GenericChange {
						span: Span::new(span.end, span.end),
						text: ")".to_string(),
					});
				}
			}
			_ => {
				// TODO
				// you could break this with ["postMessage"] etc
				// however this code only exists because of recaptcha whatever
				// and it would slow down js execution a lot
			}
		}

		walk::walk_member_expression(self, it);
	}
	// fn visit_this_expression(&mut self, it: &ThisExpression) {
	// 	self.jschanges.push(JsChange::GenericChange {
	// 		span: it.span,
	// 		text: format!("{}(this)", self.config.wrapthisfn),
	// 	});
	// }

	fn visit_debugger_statement(&mut self, it: &DebuggerStatement) {
		// delete debugger statements entirely. some sites will spam debugger as an anti-debugging measure, and we don't want that!
		self.jschanges.push(JsChange::GenericChange {
			span: it.span,
			text: "".to_string(),
		});
	}

	// we can't overwrite window.eval in the normal way because that would make everything an
	// indirect eval, which could break things. we handle that edge case here
	fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
		if let Expression::Identifier(s) = &it.callee {
			// if it's optional that actually makes it an indirect eval which is handled separately
			if s.name == "eval" && !it.optional {
				self.jschanges.push(JsChange::GenericChange {
					span: Span::new(s.span.start, s.span.end + 1),
					text: format!("eval({}(", self.config.rewritefn),
				});
				self.jschanges.push(JsChange::GenericChange {
					span: Span::new(it.span.end, it.span.end),
					text: ")".to_string(),
				});

				// then we walk the arguments, but not the callee, since we want it to resolve to
				// the real eval
				walk::walk_arguments(self, &it.arguments);
				return;
			}
		}
		if self.config.scramitize {
			self.jschanges.push(JsChange::GenericChange {
				span: Span::new(it.span.start, it.span.start),
				text: " $scramitize(".to_string(),
			});
			self.jschanges.push(JsChange::GenericChange {
				span: Span::new(it.span.end, it.span.end),
				text: ")".to_string(),
			});
		}
		walk::walk_call_expression(self, it);
	}

	fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
		let name = it.source.value.to_string();
		let text = self.rewrite_url(name);
		self.jschanges.push(JsChange::GenericChange {
			span: it.source.span,
			text,
		});
		walk::walk_import_declaration(self, it);
	}
	fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
		self.jschanges.push(JsChange::GenericChange {
			span: Span::new(it.span.start, it.span.start + 6),
			text: format!("({}(\"{}\"))", self.config.importfn, self.base),
		});
		walk::walk_import_expression(self, it);
	}

	fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
		let name = it.source.value.to_string();
		let text = self.rewrite_url(name);
		self.jschanges.push(JsChange::GenericChange {
			span: it.source.span,
			text,
		});
	}

	fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
		if let Some(source) = &it.source {
			let name = source.value.to_string();
			let text = self.rewrite_url(name);
			self.jschanges.push(JsChange::GenericChange {
				span: source.span,
				text,
			});
		}
		// do not walk further, we don't want to rewrite the identifiers
	}

	#[cfg(feature = "debug")]
	fn visit_try_statement(&mut self, it: &oxc::ast::ast::TryStatement<'a>) {
		// for debugging we need to know what the error was

		if self.config.capture_errors {
			if let Some(h) = &it.handler {
				if let Some(name) = &h.param {
					if let Some(name) = name.pattern.get_identifier() {
						self.jschanges.push(JsChange::GenericChange {
							span: Span::new(h.body.span.start + 1, h.body.span.start + 1),
							text: format!("$scramerr({});", name),
						});
					}
				}
			}
		}
		walk::walk_try_statement(self, it);
	}

	// fn visit_object_expression(&mut self, it: &ObjectExpression<'a>) {
	// 	for prop in &it.properties {
	// 		#[allow(clippy::single_match)]
	// 		match prop {
	// 			ObjectPropertyKind::ObjectProperty(p) => match &p.value {
	// 				Expression::Identifier(s) => {
	// 					if UNSAFE_GLOBALS.contains(&s.name.to_string().as_str()) && p.shorthand {
	// 						self.jschanges.push(JsChange::GenericChange {
	// 							span: s.span,
	// 							text: format!("{}: ({}({}))", s.name, self.config.wrapfn, s.name),
	// 						});
	// 						return;
	// 					}
	// 				}
	// 				_ => {}
	// 			},
	// 			_ => {}
	// 		}
	// 	}
	//
	// 	walk::walk_object_expression(self, it);
	// }

	fn visit_function_body(&mut self, it: &FunctionBody<'a>) {
		// tag function for use in sourcemaps
		if self.config.do_sourcemaps {
			self.jschanges.push(JsChange::SourceTag {
				tagstart: it.span.start,
			});
		}
		walk::walk_function_body(self, it);
	}

	fn visit_return_statement(&mut self, it: &ReturnStatement<'a>) {
		// if let Some(arg) = &it.argument {
		// 	self.jschanges.push(JsChange::GenericChange {
		// 		span: Span::new(it.span.start + 6, it.span.start + 6),
		// 		text: format!(" $scramdbg((()=>{{ try {{return arguments}} catch(_){{}} }})(),("),
		// 	});
		// 	self.jschanges.push(JsChange::GenericChange {
		// 		span: Span::new(expression_span(arg).end, expression_span(arg).end),
		// 		text: format!("))"),
		// 	});
		// }
		walk::walk_return_statement(self, it);
	}

	fn visit_unary_expression(&mut self, it: &UnaryExpression<'a>) {
		if matches!(it.operator, UnaryOperator::Typeof) {
			// don't walk to identifier rewrites since it won't matter
			return;
		}
		walk::walk_unary_expression(self, it);
	}

	// we don't want to rewrite the identifiers here because of a very specific edge case
	fn visit_for_in_statement(&mut self, it: &ForInStatement<'a>) {
		walk::walk_statement(self, &it.body);
	}
	fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
		walk::walk_statement(self, &it.body);
	}

	fn visit_update_expression(&mut self, _it: &UpdateExpression<'a>) {
		// then no, don't walk it, we don't care
	}

	fn visit_meta_property(&mut self, it: &MetaProperty<'a>) {
		if it.meta.name == "import" {
			self.jschanges.push(JsChange::GenericChange {
				span: it.span,
				text: format!("{}(\"{}\")", self.config.metafn, self.base),
			});
		}
	}

	fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
		#[allow(clippy::single_match)]
		match &it.left {
			AssignmentTarget::AssignmentTargetIdentifier(s) => {
				if ["location"].contains(&s.name.to_string().as_str()) {
					self.jschanges.push(JsChange::Assignment {
						name: s.name.to_string(),
						entirespan: it.span,
						rhsspan: it.right.span(),
						op: it.operator,
					});

					// avoid walking rest of tree, i would need to figure out nested rewrites
					// somehow
					return;
				}
			}
			AssignmentTarget::ArrayAssignmentTarget(_) => {
				// [location] = ["https://example.com"]
				// this is such a ridiculously specific edge case. just ignore it
				return;
			}
			_ => {
				// only walk the left side if it isn't an identifier, we can't replace the
				// identifier with a function obviously
				walk::walk_assignment_target(self, &it.left);
			}
		}
		walk::walk_expression(self, &it.right);
	}
}

// js MUST not be able to get a reference to any of these because sbx
const UNSAFE_GLOBALS: &[&str] = &[
	"window",
	"self",
	"globalThis",
	"this",
	"parent",
	"top",
	"location",
	"document",
	"eval",
	"frames",
];

fn random_string() -> String {
	use rand::{distributions::Alphanumeric, thread_rng, Rng};

	from_utf8(
		&thread_rng()
			.sample_iter(&Alphanumeric)
			.take(10)
			.collect::<Vec<u8>>(),
	)
	.unwrap()
	.to_string()
}

pub fn rewrite(
	js: &str,
	module: bool,
	url: Url,
	config: Config,
) -> Result<(Vec<u8>, Vec<OxcDiagnostic>)> {
	let allocator = Allocator::default();
	let source_type = SourceType::default();
	let ret = Parser::new(&allocator, js, source_type)
		.with_options(ParseOptions {
			parse_regular_expression: false, // default
			allow_return_outside_function: true,
			preserve_parens: true, // default
		})
		.parse();

	let program = ret.program;

	let sourcetag = random_string();

	let mut ast_pass = Rewriter {
		jschanges: Vec::new(),
		base: url,
		config,
	};

	// dbg!(&program);
	ast_pass.visit_program(&program);

	let mut hoists = String::new();
	if !module {
		for s in &program.body {
			if let Statement::VariableDeclaration(it) = s {
				if !it.kind.is_var() {
					continue;
				}
				ast_pass.jschanges.push(JsChange::GenericChange {
					span: Span::new(it.span.start, it.declarations[0].span.start),
					text: String::new(),
				});
				for d in &it.declarations {
					let Some(id) = d.id.get_identifier() else {
						continue; // destructures are todo
					};
					if d.init.is_some() {
						ast_pass.jschanges.push(JsChange::GenericChange {
							span: Span::new(d.id.span().start, d.id.span().start),
							text: "fakeVars.".to_string(),
						});
					} else {
						ast_pass.jschanges.push(JsChange::GenericChange {
							span: Span::new(d.id.span().start, d.id.span().end),
							text: format!("fakeVars.{}??=undefined", id),
						});
					}
					hoists += &format!(";fakeVars.{}??=undefined;", id);
				}
			}

			// TODO the function stuff breaks easily if you overwrite it
			if let Statement::FunctionDeclaration(it) = s {
				let Some(id) = &it.id else { continue };
				ast_pass.jschanges.push(JsChange::GenericChange {
					span: Span::new(it.span.end, it.span.end),
					text: format!(";fakeVars.{}={};", id, id),
				});
			}

			if let Statement::ClassDeclaration(it) = s {
				let Some(id) = &it.id else { continue };
				ast_pass.jschanges.push(JsChange::GenericChange {
					span: Span::new(it.span.end, it.span.end),
					text: format!(";fakeVars.{}={};", id, id),
				});
			}
		}
	}

	// sorrt changse
	ast_pass.jschanges.sort_by(|a, b| {
		let a = match a {
			JsChange::GenericChange { span, text: _ } => span.start,
			JsChange::Assignment {
				name: _,
				entirespan,
				rhsspan: _,
				op: _,
			} => entirespan.start,
			JsChange::SourceTag { tagstart } => *tagstart,
		};
		let b = match b {
			JsChange::GenericChange { span, text: _ } => span.start,
			JsChange::Assignment {
				name: _,
				entirespan,
				rhsspan: _,
				op: _,
			} => entirespan.start,
			JsChange::SourceTag { tagstart } => *tagstart,
		};
		a.cmp(&b)
	});

	let original_len = js.len();
	let mut difference = 0i32;

	for change in &ast_pass.jschanges {
		match &change {
			JsChange::GenericChange { span, text } => {
				difference += text.len() as i32 - (span.end - span.start) as i32;
			}
			JsChange::Assignment {
				name,
				entirespan,
				rhsspan: _,
				op: _,
			} => difference += entirespan.size() as i32 + name.len() as i32 + 10,
			_ => {}
		}
	}

	let size_estimate = (original_len as i32 + difference) as usize;
	let mut buffer: Vec<u8> = Vec::with_capacity(size_estimate);
	buffer.extend_from_slice(hoists.as_bytes());

	let mut sourcemap: Vec<u8> = Vec::new();
	if ast_pass.config.do_sourcemaps {
		sourcemap.reserve(size_estimate * 2);
		sourcemap.extend_from_slice(format!("{}([", ast_pass.config.pushsourcemapfn).as_bytes());
	}

	let mut offset = 0;
	for change in ast_pass.jschanges {
		match &change {
			JsChange::GenericChange { span, text } => {
				let start = span.start as usize;
				let end = span.end as usize;

				if ast_pass.config.do_sourcemaps {
					let spliced = &js[start..end];
					sourcemap.extend_from_slice(
						format!(
							"[\"{}\",{},{}],",
							json_escape_string(spliced),
							start,
							start + text.len()
						)
						.as_bytes(),
					);
				}

				buffer
					.extend_from_slice(js.get(offset..start).ok_or(RewriterError::Oob)?.as_bytes());

				buffer.extend_from_slice(text.as_bytes());
				offset = end;
			}
			JsChange::Assignment {
				name,
				entirespan,
				rhsspan,
				op,
			} => {
				let start = entirespan.start as usize;
				buffer.extend_from_slice(js[offset..start].as_bytes());

				buffer.extend_from_slice(
					format!(
						"((t)=>$scramjet$tryset({},\"{}\",t)||({}{}t))({})",
						name,
						op.as_str(),
						name,
						op.as_str(),
						&js[rhsspan.start as usize..rhsspan.end as usize]
					)
					.as_bytes(),
				);

				offset = entirespan.end as usize;
			}
			JsChange::SourceTag { tagstart } => {
				let start = *tagstart as usize;
				buffer
					.extend_from_slice(js.get(offset..start).ok_or(RewriterError::Oob)?.as_bytes());

				let inject = format!("/*scramtag {} {}*/", start, sourcetag);
				buffer.extend_from_slice(inject.as_bytes());

				offset = start;
			}
		}
	}
	buffer.extend_from_slice(js[offset..].as_bytes());

	if ast_pass.config.do_sourcemaps {
		sourcemap.extend_from_slice(b"],");
		sourcemap.extend_from_slice(b"\"");
		sourcemap.extend_from_slice(sourcetag.as_bytes());
		sourcemap.extend_from_slice(b"\");\n");

		sourcemap.extend_from_slice(&buffer);

		return Ok((sourcemap, ret.errors));
	}

	Ok((buffer, ret.errors))
}

fn json_escape_string(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\x08' => out.push_str("\\b"),
			'\x0C' => out.push_str("\\f"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			_ => out.push(c),
		}
	}
	out
}
