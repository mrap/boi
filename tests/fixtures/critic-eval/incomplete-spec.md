# BOI Template System Spec

Add a template system that lets users define reusable spec templates with variables, reducing boilerplate when creating similar specs.

## Constraints
- All code lives in ~/boi/
- Python: stdlib only
- Shell: `set -uo pipefail` (no `-e`)
- Run `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` after every task

## Tasks

### t-1: Create template parser
DONE

**Spec:** Create `~/boi/lib/template_parser.py` that parses template files with `{{variable}}` placeholders.

Functions:
- `parse_template(template_path)` - Read template and extract variable names
- `render_template(template_content, variables)` - Replace variables with values
- `validate_variables(template_content, provided_vars)` - Check all required vars are provided

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-2: Add template storage
DONE

**Spec:** Create template storage at `~/.boi/templates/` with save, list, and delete operations.

Functions:
- `save_template(name, content, templates_dir)` - Save a template file
- `list_templates(templates_dir)` - Return all available templates
- `load_template(name, templates_dir)` - Load a template by name
- `delete_template(name, templates_dir)` - Remove a template

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-3: Implement template CLI commands

### t-4: Add template variables wizard
DONE

**Spec:** Create an interactive wizard that prompts users for template variables when creating a spec from a template.

Functions:
- `collect_variables(template_content)` - Extract all `{{var}}` placeholders
- `prompt_for_variables(var_names)` - Interactively ask user for values
- `create_spec_from_template(template_name, variables, output_path)` - Generate spec file

**Verify:** `cd ~/boi && python3 -m unittest discover -s tests -p 'test_*.py'` passes.

### t-5: Write template tests
SKIPPED

**Verify:** Tests pass.
