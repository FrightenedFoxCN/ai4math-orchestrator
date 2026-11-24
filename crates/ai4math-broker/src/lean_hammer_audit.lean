import Lean
import Lean.Replay

open Lean

-- Names arrive as data, not from term syntax, aliases, or namespace resolution.
private def exactName (text : String) : Name :=
  (text.splitOn ".").foldl Name.str .anonymous

private def moduleOf (env : Environment) (name : Name) : Option Name := do
  let idx ← env.getModuleIdxFor? name
  env.header.moduleNames[idx.toNat]?

private def ownTheorem (objects : System.FilePath) (env : Environment)
    (mod name : Name) : IO Unit := do
  let invalid := IO.userError s!"'{name}' is not a public theorem of module '{mod}'"
  let some idx := env.getModuleIdxFor? name | throw invalid
  unless !isPrivateName name && env.header.moduleNames[idx.toNat]? == some mod do
    throw invalid
  let some data := env.header.moduleData[idx.toNat]? | throw invalid
  let some info@(.thmInfo _) := data.constants.find? (·.name == name) | throw invalid
  unless env.find? name == some info do
    throw <| IO.userError s!"ambiguous kernel theorem '{name}'"
  let (exported, _) ← Lean.readModuleData (objects / (mod.toString ++ ".olean"))
  let some publicInfo := exported.constants.find? (·.name == name) | throw invalid
  unless publicInfo.toConstantVal == info.toConstantVal do
    throw invalid

-- An imported object is not a kernel check. Replay the exact theorem and the
-- declarations it actually uses, under stock kernel options, not source options.
private partial def replayClosure (stock imported : Environment) (todo : List Name)
    (acc : Std.HashMap Name ConstantInfo := {}) : IO (Std.HashMap Name ConstantInfo) := do
  match todo with
  | [] => return acc
  | name :: rest =>
    if acc.contains name then
      replayClosure stock imported rest acc
    else if let some expected := stock.find? name then
      unless moduleOf imported name == moduleOf stock name && imported.find? name == some expected do
        throw <| IO.userError s!"source shadows installed Lean constant '{name}'"
      replayClosure stock imported rest acc
    else
      let some info := imported.find? name
        | throw <| IO.userError s!"missing kernel declaration '{name}'"
      if info.isUnsafe || info.isPartial then
        throw <| IO.userError s!"unsafe or partial dependency '{name}'"
      if let .axiomInfo _ := info then
        throw <| IO.userError s!"non-kernel axiom '{name}'"
      let deps := info.getUsedConstantsAsSet.toList ++ (match info with
        | .inductInfo v => v.all
        | .ctorInfo v => [v.induct]
        | _ => [])
      replayClosure stock imported (deps ++ rest) (acc.insert name info)

def main (args : List String) : IO Unit := do
  let objectsText :: modText :: targetText :: hints := args
    | throw <| IO.userError "missing Lean hammer audit arguments"
  let objects := System.FilePath.mk objectsText
  let mod := Name.mkSimple modText
  let target := exactName targetText
  let builtins : Array Import := #[{ module := `Lean }]
  let stock ← Lean.importModules builtins {} (loadExts := false)
  let builtinPath ← Lean.searchPathRef.get
  Lean.searchPathRef.set (builtinPath ++ [objects])
  let imported ← Lean.importModules (builtins.push { module := mod }) {} (loadExts := false)
  ownTheorem objects imported mod target
  for text in hints do
    let name := exactName text
    let some info := imported.find? name
      | throw <| IO.userError s!"unknown exact Lean hammer premise '{name}'"
    if isPrivateName name || info.isUnsafe || info.isPartial then
      throw <| IO.userError s!"invalid Lean hammer premise '{name}'"
  let closure ← replayClosure stock imported [target]
  let checked ← stock.toKernelEnv.replay closure
  let collect : CoreM (Array Name) := Lean.collectAxioms target
  let (axioms, _) ← collect.toIO
    { fileName := "<Lean hammer audit>", fileMap := default } { env := .ofKernelEnv checked }
  for ax in axioms do
    unless ax == `propext || ax == `Classical.choice || ax == `Quot.sound do
      throw <| IO.userError s!"non-L5 Lean hammer axiom '{ax}'"
  IO.println ("ai4math:sealed-lean-hammer-audit:v1 " ++
    (Lean.toJson ((axioms.map Name.toString).qsort (· < ·))).compress)
