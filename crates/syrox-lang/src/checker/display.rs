use crate::{ResolvedProgram, Ty};

impl ResolvedProgram {
    /// Bounded, human-readable type presentation shared by diagnostics and IDEs.
    /// This is a label, not necessarily an accessible type expression at a use site.
    pub fn display_type(&self, ty: &Ty) -> String {
        fn write(
            program: &ResolvedProgram,
            ty: &Ty,
            out: &mut String,
            nodes: &mut usize,
            depth: usize,
        ) {
            if *nodes == 0 || depth >= 32 || out.len() >= 2048 {
                if !out.ends_with('…') {
                    out.push('…');
                }
                return;
            }
            *nodes -= 1;
            match ty {
                Ty::Unit => out.push_str("unit"),
                Ty::Int => out.push_str("int"),
                Ty::Str => out.push_str("str"),
                Ty::Error => out.push_str("unknown"),
                Ty::Parameter(id) => {
                    let name = program
                        .locals()
                        .nth(id.index())
                        .map_or("?", |local| local.name());
                    out.extend(name.chars().take(256));
                    if name.len() > 256 {
                        out.push('…');
                    }
                }
                Ty::Nominal(id) | Ty::Specialization { template: id, .. } => {
                    if let Some(item) = program.items().nth(id.index()) {
                        for (i, part) in item.path().segments().iter().enumerate() {
                            if out.len() >= 2048 {
                                out.push('…');
                                break;
                            }
                            if i > 0 {
                                out.push_str("::");
                            }
                            out.extend(part.chars().take(256));
                            if part.len() > 256 {
                                out.push('…');
                            }
                        }
                        if program.ambiguous_type_name(*id) {
                            use std::fmt::Write as _;
                            let _ = write!(out, " [domain {}]", item.domain().as_u32());
                        }
                    } else {
                        out.push('?');
                    }
                    if let Ty::Specialization { arguments, .. } = ty {
                        out.push('<');
                        list(program, arguments, out, nodes, depth);
                        out.push('>');
                    }
                }
                Ty::List(inner) => {
                    out.push('[');
                    write(program, inner, out, nodes, depth + 1);
                    out.push(']');
                }
                Ty::Function {
                    parameters,
                    result,
                    once,
                } => {
                    out.push_str(if *once { "once fn(" } else { "fn(" });
                    list(program, parameters, out, nodes, depth);
                    out.push_str(") -> ");
                    write(program, result, out, nodes, depth + 1);
                }
            }
        }
        fn list(
            program: &ResolvedProgram,
            types: &[Ty],
            out: &mut String,
            nodes: &mut usize,
            depth: usize,
        ) {
            for (i, ty) in types.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(program, ty, out, nodes, depth + 1);
                if *nodes == 0 || out.len() >= 2048 {
                    break;
                }
            }
        }
        let mut text = String::new();
        write(self, ty, &mut text, &mut 128, 0);
        text
    }
}
