# Fut

Fut organizes project work into terminal contexts. A saved layout remembers that arrangement without preserving running program sessions.

## Language

**Project**:
A repository or directory whose work belongs together. Linked checkouts of the same repository belong to one project.

**Project session**:
The live project-level container for workspaces.
_Avoid_: Window, agent session

**Workspace**:
An ordered working context within a project session, associated with a checkout or directory and containing tabs.

**Tab**:
An ordered terminal layout within a workspace, containing one or more panes.
_Avoid_: Window

**Pane**:
A placement of one terminal within a tab's layout.

**Project recipe**:
The declared starting arrangement, commands, environment, and extension settings for a project session.

**Saved layout**:
A remembered arrangement of project sessions, workspaces, tabs, and panes, together with their directories, split proportions, and program start commands.
_Avoid_: Session backup

**Checkpoint**:
The latest explicitly or automatically remembered arrangement of a project's workspaces, tabs, panes, and program start commands.

**Restart command**:
The executable and arguments used to start a fresh foreground program in a saved pane's working directory. It does not represent terminal input history or a collection of background jobs.

**Restart approval**:
The user's permission to automatically execute a saved restart command. Remembering a command does not itself grant that permission.

**Restore**:
Recreating a saved layout with newly started programs rather than continuing their previous running sessions. Agent-session resume is a separate action chosen by the user.
