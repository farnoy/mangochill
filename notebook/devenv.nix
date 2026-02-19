{
  ...
}:

{
  languages.python = {
    enable = true;
    uv = {
      enable = true;
      sync.enable = true;
    };
    venv.enable = true;
  };

  processes.notebook.exec = "uv run marimo edit analysis.py --watch";
  scripts.notebook.exec = "marimo edit analysis.py --watch";
}
