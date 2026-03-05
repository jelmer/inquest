//! Quickstart documentation for inquest

use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

/// Command to display quickstart documentation for inquest.
///
/// Shows introductory documentation to help users get started with
/// basic inquest operations.
pub struct QuickstartCommand;

impl Default for QuickstartCommand {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickstartCommand {
    /// Creates a new quickstart command.
    pub fn new() -> Self {
        QuickstartCommand
    }
}

impl Command for QuickstartCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let help = r#"# Inquest

## Overview

This project provides a database of test results which can be used as part of
developer workflow to ensure/check things like:

* No commits without having had a test failure, test fixed cycle.
* No commits without new tests being added.
* What tests have failed since the last commit (to run just a subset).
* What tests are currently failing and need work.

Test results are inserted using subunit (and thus anything that can output
subunit or be converted into a subunit stream can be accepted).

## Licensing

Inquest is under BSD / Apache 2.0 licences.

## Quick Start

Create a config file:

```sh
$ touch .testr.conf
```

Create a repository:

```sh
$ inq init
```

Load a test run into the repository:

```sh
$ inq load < testrun
```

Query the repository:

```sh
$ inq stats
$ inq last
$ inq failing
```

Delete a repository:

```sh
$ rm -rf .testrepository
```

## Documentation

More detailed documentation can be found in the original Python version at
https://testing-cabal.github.io/testrepository/
"#;
        ui.output(help)?;
        Ok(0)
    }

    fn name(&self) -> &str {
        "quickstart"
    }

    fn help(&self) -> &str {
        "Show quickstart documentation for inquest"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_ui::TestUI;

    #[test]
    fn test_quickstart_command() {
        let mut ui = TestUI::new();
        let cmd = QuickstartCommand::new();
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert!(!ui.output.is_empty());
        let output = ui.output.join("\n");
        assert!(output.contains("Quick Start"));
        assert!(output.contains("inq init"));
        assert!(output.contains("inq load"));
    }
}
