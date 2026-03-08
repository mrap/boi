# Projects

Projects group related specs and inject shared context into every worker prompt. Use projects when you have multiple specs that work within the same codebase or problem domain and need shared knowledge.

## Creating a Project

```bash
boi project create ios-app --description "iOS app rewrite"
```

This creates `~/.boi/projects/ios-app/` with:
- `project.json` - Metadata (name, description, defaults)
- `context.md` - Shared context injected into worker prompts (starts empty)

Project names must be alphanumeric with hyphens only (no spaces).

## Dispatching into a Project

```bash
boi dispatch --spec feature.md --project ios-app
```

Workers on this spec automatically receive two additional files in their prompt:
- **`context.md`**: Your project-level context (architecture notes, conventions, patterns)
- **`research.md`**: Discoveries accumulated by workers across iterations

This means every worker starts with project-specific knowledge, even though it has no memory of previous sessions.

## Writing Context

Edit the project's `context.md` to include information workers need:

```markdown
# ios-app Context

## Architecture
- SwiftUI views in `Sources/Views/`
- ViewModels in `Sources/ViewModels/`
- Network layer in `Sources/Network/`
- All API calls go through `APIClient.swift`

## Conventions
- Use async/await for network calls
- ViewModels are `@Observable` classes
- All new views need previews
- Tests use XCTest, mocks in `Tests/Mocks/`

## Key Decisions
- Using URLSession directly, not Alamofire
- Navigation via NavigationStack, not NavigationView
- State management with @Observable, not Combine
```

## Research File

Workers can append discoveries to `~/.boi/projects/{name}/research.md` during execution. This builds up institutional knowledge across iterations:

```markdown
# Research

## Iteration 3 (q-001, t-2)
- The `APIClient` uses a custom `RequestBuilder` pattern. New endpoints
  should follow the pattern in `UserEndpoint.swift`.

## Iteration 7 (q-001, t-5)
- Found that the auth token refresh logic is in `AuthInterceptor.swift`,
  not in `APIClient`. All authenticated requests pass through this interceptor.
```

Future workers on any spec in this project receive this research automatically.

## Managing Projects

### List all projects

```bash
boi project list
boi project list --json    # Machine-readable
```

Shows each project with its description and number of associated specs.

### View project status

```bash
boi project status ios-app
boi project status ios-app --json
```

Shows metadata and all associated specs with their current status.

### Print context

```bash
boi project context ios-app
```

Prints the contents of `context.md` to stdout.

### Delete a project

```bash
boi project delete ios-app
```

Confirms before deleting. Does NOT cancel running specs associated with the project.

## Directory Structure

```
~/.boi/projects/
  ios-app/
    project.json    # Name, description, defaults
    context.md      # Shared context for worker prompts
    research.md     # Auto-populated discoveries
  backend-api/
    project.json
    context.md
    research.md
```

## Project Defaults

The `project.json` file can include default settings applied to all specs dispatched into the project:

```json
{
  "name": "ios-app",
  "description": "iOS app rewrite",
  "created_at": "2026-03-01T10:00:00+00:00",
  "default_priority": 100,
  "default_max_iter": 30,
  "tags": []
}
```

## When to Use Projects

**Use projects when:**
- Multiple specs work in the same codebase
- Workers need shared architectural knowledge
- You want discoveries from one spec to inform future specs
- You're working on a long-running initiative with many specs over time

**Skip projects when:**
- A one-off spec with no expected follow-up
- The spec is self-contained and doesn't need shared context
- You're running a quick experiment
