import Lean

open Lean Lean.Elab

private def validModule (text : String) : Bool :=
  match text.toList with
  | [] => false
  | first :: rest =>
    ('A' ≤ first && first ≤ 'Z') &&
      rest.all (fun c => ('A' ≤ c && c ≤ 'Z') || ('a' ≤ c && c ≤ 'z') ||
        ('0' ≤ c && c ≤ '9') || c == '_')

private def discardOutput : IO.FS.Stream where
  flush := pure ()
  read := fun _ => pure ByteArray.empty
  write := fun _ => pure ()
  getLine := pure ""
  putStr := fun _ => pure ()
  isTty := pure false

private def opts : Options := Elab.async.set {} false
private def maxCommands : Nat := 1000000
private def maxRaw : Nat := 9007199254740991 -- JSON's largest safe integer (2^53 - 1).

private structure Sample where
  count : Nat := 0
  peak : Nat := 0
  total : Nat := 0

private def checkSynchronous (s : Command.State) : IO Unit := do
  if s.scopes.any (fun scope => Elab.async.get scope.opts) || !s.snapshotTasks.isEmpty then
    throw <| IO.userError "heartbeat probe cannot measure asynchronous elaboration"

private def enablesAsync (stx : Syntax) : Bool :=
  (stx.isOfKind ``Parser.Command.set_option || stx.isOfKind ``Parser.Term.set_option ||
    stx.isOfKind ``Parser.Tactic.set_option) &&
    stx[1].getId.eraseMacroScopes == `Elab.async && stx[3].getAtomVal == "true"

private def preflight : Frontend.FrontendM Unit := do
  let cs ← Frontend.getCommandState
  checkSynchronous cs
  let scope := cs.scopes.head!
  let ictx ← Frontend.getInputContext
  let ps ← Frontend.getParserState
  -- In Lean 4.35, processCommand's elaborator resets the message log, including parser
  -- errors. Check the same parse separately; only processCommand itself is measured.
  let (stx, _, messages) := Parser.parseCommand ictx
    { env := cs.env, options := scope.opts, currNamespace := scope.currNamespace,
      openDecls := scope.openDecls } ps {}
  if messages.hasErrors then
    throw <| IO.userError "heartbeat probe target has parser errors"
  -- Reject explicit enabling syntax, even local `set_option ... in` which would restore
  -- the option before the post-command check. Macros/custom elaborators may still change
  -- local options, spawn their own tasks, or even adjust the counter; this is not attestation.
  if (stx.find? enablesAsync).isSome then
    throw <| IO.userError "heartbeat probe does not support enabling Elab.async"

private def measureCommands : Frontend.FrontendM Sample := do
  let mut sample : Sample := {}
  repeat
    preflight
    let before ← IO.getNumHeartbeats
    let terminal ← Frontend.processCommand
    let after ← IO.getNumHeartbeats
    let st ← get
    checkSynchronous st.commandState
    if st.commandState.messages.hasErrors then
      throw <| IO.userError "heartbeat probe target has elaboration errors"
    if after < before then
      throw <| IO.userError "heartbeat probe counter decreased"
    if terminal then
      -- A delayed import or an explicit exit must not yield a partial-file sample.
      unless st.commands.back!.isOfKind ``Parser.Command.eoi do
        throw <| IO.userError "heartbeat probe target terminated before EOF"
      break
    let raw := after - before
    let count := sample.count + 1
    let total := sample.total + raw
    if count > maxCommands || total > maxRaw then
      throw <| IO.userError "heartbeat probe sample exceeds bounds"
    sample := { count, peak := max sample.peak raw, total }
    -- processCommand retains syntax in this array; only the most recent entry is needed.
    modify fun s => { s with commands := #[] }
  if sample.count == 0 then
    throw <| IO.userError "heartbeat probe target has no commands"
  return sample

private def probe (sourcePath : String) (moduleName : Name) : IO Sample := do
  let input ← IO.FS.readFile ⟨sourcePath⟩
  let ictx := Parser.mkInputContext input sourcePath
  let (header, parserState, messages) ← Parser.parseHeader ictx
  if messages.hasErrors then
    throw <| IO.userError "heartbeat probe target has an invalid import header"
  -- Same trusted import API as Lean, using the caller's validated read-only LEAN_PATH.
  -- The CLI consumed/reset the initialization flag while processing this wrapper's imports.
  unsafe enableInitializersExecution
  let (env, messages) ← processHeader ⟨header⟩ opts messages ictx
    (leakEnv := true) (mainModule := moduleName)
  if messages.hasErrors then
    throw <| IO.userError "heartbeat probe target imports failed"
  let commandState := Command.mkState env {} opts
  let (sample, _) ← measureCommands.run { inputCtx := ictx } |>.run { commandState, parserState, cmdPos := parserState.pos }
  return sample

def main (args : List String) : IO Unit := do
  let [sourcePath, moduleText] := args
    | throw <| IO.userError "heartbeat probe expects SOURCE_PATH MODULE_NAME"
  unless validModule moduleText do
    throw <| IO.userError "heartbeat probe expects a bare module name"
  let sample ← IO.withStdout discardOutput (probe sourcePath (Name.mkSimple moduleText))
  IO.println s!"ai4math:t2-heartbeats:v1 {sample.count} {sample.peak} {sample.total}"
