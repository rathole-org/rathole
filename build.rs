use anyhow::Result;
use vergen::{BuildBuilder, CargoBuilder, Emitter, RustcBuilder, SysinfoBuilder};
use vergen_gitcl::GitclBuilder;

fn main() -> Result<()> {
    let build = BuildBuilder::all_build()?;
    let cargo = CargoBuilder::all_cargo()?;
    let rustc = RustcBuilder::all_rustc()?;
    let si = SysinfoBuilder::all_sysinfo()?;

    let mut git = GitclBuilder::default();
    git.describe(false, true, Some("-dirty"))
        .sha(false)
        .commit_timestamp(true)
        .branch(true);
    let git = git.build()?;

    if let Err(e) = Emitter::default()
        .add_instructions(&build)?
        .add_instructions(&cargo)?
        .add_instructions(&rustc)?
        .add_instructions(&si)?
        .add_instructions(&git)?
        .emit()
    {
        eprintln!("error occurred while generating instructions: {:?}", e);
        Emitter::default()
            .add_instructions(&build)?
            .add_instructions(&cargo)?
            .add_instructions(&rustc)?
            .add_instructions(&si)?
            .emit()
    } else {
        Ok(())
    }
}
