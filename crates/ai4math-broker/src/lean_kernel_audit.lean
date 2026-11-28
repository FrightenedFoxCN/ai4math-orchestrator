import Lean
import Lean.Replay

open Lean

-- Names arrive as data, never as candidate term syntax or namespace aliases.
private def exactName (text : String) : Name :=
  (text.splitOn ".").foldl Name.str .anonymous

private def moduleOf (env : Environment) (name : Name) : Option Name := do
  let idx ← env.getModuleIdxFor? name
  env.header.moduleNames[idx.toNat]?

-- Bind the requested root to the exact public declaration in its own object.
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

-- Object import is not a kernel check. Replay only declarations reachable from
-- the requested roots, under stock kernel options rather than source options.
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
      let partialImpl := (imported.find? (Compiler.mkUnsafeRecName name)).any ConstantInfo.isPartial
      if info.isUnsafe || info.isPartial || partialImpl then
        throw <| IO.userError s!"unsafe or partial dependency '{name}'"
      if let .axiomInfo _ := info then
        throw <| IO.userError s!"non-kernel axiom '{name}'"
      let deps := info.getUsedConstantsAsSet.toList ++ (match info with
        | .inductInfo v => v.all
        | .ctorInfo v => [v.induct]
        | _ => [])
      replayClosure stock imported (deps ++ rest) (acc.insert name info)

private def auditAxioms (env : Environment) (roots : List Name) (allowSorry : Bool) : IO (Array String) := do
  let mut observed : Std.HashSet Name := {}
  for name in roots do
    let collect : CoreM (Array Name) := Lean.collectAxioms name
    let (axioms, _) ← collect.toIO
      { fileName := "<independent Lean kernel replay>", fileMap := default } { env }
    for ax in axioms do
      unless ax == `propext || ax == `Classical.choice || ax == `Quot.sound || (allowSorry && ax == `sorryAx) do
        throw <| IO.userError s!"'{name}' depends on non-kernel axiom '{ax}'"
      observed := observed.insert ax
  return (observed.toArray.map Name.toString).qsort (· < ·)

private def replayAndSeal (stock imported : Environment) (roots : List Name) (allowSorry : Bool) : IO Unit := do
  let closure ← replayClosure stock imported roots
  let checked ← stock.toKernelEnv.replay closure
  let axioms ← auditAxioms (.ofKernelEnv checked) roots allowSorry
  IO.println ("ai4math:sealed-lean-kernel-audit:v1 " ++
    (Lean.toJson axioms).compress)

private def runTarget (args : List String) (allowSorry : Bool) : IO Unit := do
  let [objectsText, moduleText, targetText] := args
    | throw <| IO.userError "invalid Lean kernel target replay arguments"
  let objects := System.FilePath.mk objectsText
  let mod := Name.mkSimple moduleText
  let target := exactName targetText
  let builtins : Array Import := #[{ module := `Lean }]
  let stock ← Lean.importModules builtins {} (loadExts := false)

  -- Temporary objects are data only: no candidate parser, elaborator,
  -- environment extension, or initializer participates in this checker.
  let builtinPath ← Lean.searchPathRef.get
  Lean.searchPathRef.set (builtinPath ++ [objects])
  let imported ← Lean.importModules (builtins.push { module := mod }) {} (loadExts := false)
  ownTheorem objects imported mod target
  replayAndSeal stock imported [target] allowSorry

private def publicRoots (objects : System.FilePath) (imported : Environment)
    (mods : List Name) : IO (List Name) := do
  let mut roots := []
  for mod in mods do
    let (exported, _) ← Lean.readModuleData (objects / (mod.toString ++ ".olean"))
    for info in exported.constants do
      unless isPrivateName info.name do
        unless moduleOf imported info.name == some mod && imported.find? info.name == some info do
          throw <| IO.userError s!"ambiguous public declaration '{info.name}' in module '{mod}'"
        let partialImpl := (imported.find? (Compiler.mkUnsafeRecName info.name)).any ConstantInfo.isPartial
        if info.isUnsafe || info.isPartial || partialImpl then
          throw <| IO.userError s!"unsafe or partial public declaration '{info.name}'"
        if let .axiomInfo _ := info then
          throw <| IO.userError s!"non-kernel public axiom '{info.name}'"
        roots := info.name :: roots
  return roots

private def runModules (args : List String) : IO Unit := do
  let objectsText :: countText :: moduleTexts := args
    | throw <| IO.userError "invalid Lean kernel module replay arguments"
  let some count := countText.toNat?
    | throw <| IO.userError "invalid Lean kernel module count"
  unless count > 0 && count == moduleTexts.length do
    throw <| IO.userError "Lean kernel module count does not match the module list"
  let objects := System.FilePath.mk objectsText
  let mods := moduleTexts.map Name.mkSimple
  let builtins : Array Import := #[{ module := `Lean }]
  let stock ← Lean.importModules builtins {} (loadExts := false)
  let builtinPath ← Lean.searchPathRef.get
  Lean.searchPathRef.set (builtinPath ++ [objects])
  let imports := builtins ++ (mods.toArray.map fun mod => { module := mod })
  let imported ← Lean.importModules imports {} (loadExts := false)
  let roots ← publicRoots objects imported mods
  replayAndSeal stock imported roots false

def main (args : List String) : IO Unit := do
  match args with
  | "target" :: rest => runTarget rest false
  | "sketch-target" :: rest => runTarget rest true
  | "modules" :: rest => runModules rest
  | _ => throw <| IO.userError "invalid Lean kernel replay mode"
