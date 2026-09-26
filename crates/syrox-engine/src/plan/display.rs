//! Streaming, byte-bounded rendering of a projected Plan.

use super::{MAX_PLAN_DISPLAY_BYTES, Plan, PlanError, PlanType, PlanValue, fmt};

impl fmt::Display for Plan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.canonical_display_bytes > MAX_PLAN_DISPLAY_BYTES {
            return Err(fmt::Error);
        }
        let mut formatter = ByteLimitedWriter::new(formatter, self.canonical_display_bytes);
        display_plan(self, &mut formatter)
    }
}

fn display_plan<W: fmt::Write + ?Sized>(plan: &Plan, formatter: &mut W) -> fmt::Result {
    writeln!(formatter, "syrox plan")?;
    write!(formatter, "lock sha256 ")?;
    display_digest(formatter, &plan.lock_digest)?;
    writeln!(formatter)?;
    writeln!(formatter, "policy {}", plan.policy_identity)?;
    if let Some(standard) = &plan.standard_library {
        formatter.write_str("std sha256 ")?;
        display_digest(formatter, &standard.digest)?;
        writeln!(formatter)?;
    } else {
        writeln!(formatter, "std absent")?;
    }
    writeln!(formatter, "package dependency graph")?;
    writeln!(
        formatter,
        "packages {} edges {}",
        plan.packages.len(),
        plan.package_edges
    )?;
    for package in &plan.packages {
        write!(formatter, "package ")?;
        display_string(formatter, package.id.as_str())?;
        if let Some(export) = &package.export {
            formatter.write_str(" export ")?;
            display_string(formatter, export)?;
        }
        writeln!(formatter)?;
        for dependency in &package.dependencies {
            write!(formatter, "  dependency ")?;
            display_string(formatter, dependency.as_str())?;
            writeln!(formatter)?;
        }
    }
    writeln!(formatter, "acquisitions {}", plan.acquisitions.len())?;
    for acquisition in &plan.acquisitions {
        write!(formatter, "acquisition ")?;
        display_string(formatter, acquisition.package.as_str())?;
        writeln!(formatter, " sources {}", acquisition.sources.len())?;
        for source in &acquisition.sources {
            formatter.write_str("  source ")?;
            display_string(formatter, &source.url)?;
            writeln!(
                formatter,
                " sha256 {} max {}",
                source.digest, source.maximum_bytes
            )?;
        }
    }
    writeln!(formatter, "builds {}", plan.builds.len())?;
    for build in &plan.builds {
        formatter.write_str("build ")?;
        display_string(formatter, build.package().as_str())?;
        writeln!(formatter, " protocol {}", build.protocol())?;
        formatter.write_str("  source-directory ")?;
        display_string(formatter, build.source_directory())?;
        formatter.write_str(" entry ")?;
        display_string(formatter, build.entry())?;
        writeln!(formatter, " timeout {}", build.timeout_seconds())?;
        if let Some(provider) = build.development() {
            formatter.write_str("  build-input ")?;
            display_string(formatter, provider.as_str())?;
            writeln!(formatter, " dev")?;
        }
    }
    if let Some(default) = &plan.default_build {
        formatter.write_str("default-build ")?;
        display_string(formatter, default.as_str())?;
        writeln!(formatter)?;
    }
    display_applications(plan, formatter)?;
    for root in &plan.roots {
        write!(formatter, "root {}:", root.domain)?;
        display_path(formatter, &root.path)?;
        writeln!(formatter, " ({})", root.name)?;
        writeln!(formatter, "  type: {}", DisplayType(&root.ty))?;
        writeln!(formatter, "  value: {}", DisplayValue(&root.value))?;
        for claim in &root.claims {
            write!(
                formatter,
                "  claim: {} = {} @ {}:",
                DisplayType(&claim.ty),
                DisplayValue(&claim.value),
                claim.scope_root_domain
            )?;
            display_path(formatter, &claim.scope_root_path)?;
            if let Some(name) = &claim.scope_name {
                write!(formatter, "/{name}")?;
            }
            writeln!(formatter, "#{}", claim.boundary)?;
        }
    }
    Ok(())
}

fn display_applications<W: fmt::Write + ?Sized>(plan: &Plan, formatter: &mut W) -> fmt::Result {
    writeln!(formatter, "applications {}", plan.applications.len())?;
    for app in &plan.applications {
        formatter.write_str("application ")?;
        display_string(formatter, app.package().as_str())?;
        writeln!(formatter)?;
        if let Some(loader) = app.loader() {
            formatter.write_str("  loader ")?;
            display_string(formatter, loader.as_str())?;
            writeln!(formatter)?;
        }
        for library in app.libraries() {
            formatter.write_str("  library ")?;
            display_string(formatter, library.as_str())?;
            writeln!(formatter)?;
        }
    }
    if let Some(default) = &plan.default_application {
        formatter.write_str("default-application ")?;
        display_string(formatter, default.as_str())?;
        writeln!(formatter)?;
    }
    Ok(())
}

fn display_digest<W: fmt::Write + ?Sized>(formatter: &mut W, digest: &[u8; 32]) -> fmt::Result {
    for byte in digest {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

struct DisplayType<'a>(&'a PlanType);

impl fmt::Display for DisplayType<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            PlanType::Unit => formatter.write_str("unit"),
            PlanType::Int => formatter.write_str("int"),
            PlanType::Str => formatter.write_str("str"),
            PlanType::Nominal { domain, path } => {
                write!(formatter, "{domain}:")?;
                display_path(formatter, path)
            }
            PlanType::Specialization {
                domain,
                path,
                arguments,
            } => {
                write!(formatter, "{domain}:")?;
                display_path(formatter, path)?;
                formatter.write_str("<")?;
                for (index, argument) in arguments.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{}", DisplayType(argument))?;
                }
                formatter.write_str(">")
            }
            PlanType::List(item) => write!(formatter, "[{}]", DisplayType(item)),
        }
    }
}

struct DisplayValue<'a>(&'a PlanValue);

impl fmt::Display for DisplayValue<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            PlanValue::Unit => formatter.write_str("()"),
            PlanValue::Int(value) => write!(formatter, "{value}"),
            PlanValue::Str(value) => display_string(formatter, value),
            PlanValue::Nominal { ty, value, .. } => {
                write!(formatter, "{}({})", DisplayType(ty), DisplayValue(value))
            }
            PlanValue::List { items, .. } => {
                formatter.write_str("[")?;
                display_values(formatter, items)?;
                formatter.write_str("]")
            }
            PlanValue::Struct { fields, .. } => {
                formatter.write_str("{")?;
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{name} = {}", DisplayValue(value))?;
                }
                formatter.write_str("}")
            }
            PlanValue::Variant { ty, index } => write!(formatter, "{}::{index}", DisplayType(ty)),
        }
    }
}

fn display_values(formatter: &mut fmt::Formatter<'_>, values: &[PlanValue]) -> fmt::Result {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            formatter.write_str(", ")?;
        }
        write!(formatter, "{}", DisplayValue(value))?;
    }
    Ok(())
}

fn display_path<W: fmt::Write + ?Sized>(formatter: &mut W, path: &[String]) -> fmt::Result {
    for (index, component) in path.iter().enumerate() {
        if index > 0 {
            formatter.write_str("::")?;
        }
        formatter.write_str(component)?;
    }
    Ok(())
}

fn display_string<W: fmt::Write + ?Sized>(formatter: &mut W, value: &str) -> fmt::Result {
    formatter.write_char('"')?;
    for character in value.chars() {
        match character {
            '"' => formatter.write_str("\\\"")?,
            '\\' => formatter.write_str("\\\\")?,
            '\n' => formatter.write_str("\\n")?,
            '\r' => formatter.write_str("\\r")?,
            '\t' => formatter.write_str("\\t")?,
            character if character.is_control() => {
                for escaped in character.escape_default() {
                    formatter.write_char(escaped)?;
                }
            }
            character => formatter.write_char(character)?,
        }
    }
    formatter.write_char('"')
}

#[derive(Debug)]
struct ByteLimitedWriter<W> {
    inner: W,
    bytes: usize,
    limit: usize,
}

impl<W> ByteLimitedWriter<W> {
    const fn new(inner: W, limit: usize) -> Self {
        Self {
            inner,
            bytes: 0,
            limit,
        }
    }
}

impl<W: fmt::Write> fmt::Write for ByteLimitedWriter<W> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let Some(bytes) = self.bytes.checked_add(value.len()) else {
            return Err(fmt::Error);
        };
        if bytes > self.limit {
            return Err(fmt::Error);
        }
        self.inner.write_str(value)?;
        self.bytes = bytes;
        Ok(())
    }
}

#[derive(Debug)]
struct ByteCounter {
    bytes: usize,
    limit: usize,
}

impl fmt::Write for ByteCounter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let Some(bytes) = self.bytes.checked_add(value.len()) else {
            return Err(fmt::Error);
        };
        if bytes > self.limit {
            return Err(fmt::Error);
        }
        self.bytes = bytes;
        Ok(())
    }
}

pub(super) fn canonical_display_bytes(plan: &Plan, limit: usize) -> Result<usize, PlanError> {
    let mut counter = ByteCounter { bytes: 0, limit };
    display_plan(plan, &mut counter).map_err(|_| PlanError::DisplayBytesLimit { limit })?;
    Ok(counter.bytes)
}
