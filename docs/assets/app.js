// PiForge docs — install tab switcher + copy-to-clipboard.
document.addEventListener("DOMContentLoaded", () => {
  // Install command tabs.
  const tabs = document.querySelectorAll(".install-tab");
  const cmdEl = document.querySelector(".code-block .cmd");
  const commands = {
    go: "go install github.com/nitishagar/piforge/cmd/piforge@latest",
    build:
      "git clone https://github.com/nitishagar/piforge && cd piforge && make build-arm64",
    curl: "curl -fsSL https://github.com/nitishagar/piforge/raw/main/dist/piforge-arm64 -o piforge && chmod +x piforge",
  };
  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      tabs.forEach((t) => t.classList.remove("active"));
      tab.classList.add("active");
      const key = tab.dataset.cmd;
      if (commands[key]) cmdEl.textContent = commands[key];
    });
  });

  // Copy buttons.
  document.querySelectorAll(".copy-btn").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const text = btn.parentElement.querySelector(".cmd").textContent.trim();
      try {
        await navigator.clipboard.writeText(text);
        const original = btn.textContent;
        btn.textContent = "copied";
        btn.classList.add("copied");
        setTimeout(() => {
          btn.textContent = original;
          btn.classList.remove("copied");
        }, 1500);
      } catch {
        // Clipboard may be blocked; no-op.
      }
    });
  });
});
