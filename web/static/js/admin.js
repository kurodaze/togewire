(function () {
  // Codes, not text, so a crafted link cannot show its own message here.
  const NOTICES = new Map([
    ["paired", "Paired with Spotify"],
    ["disconnected", "Disconnected from Spotify"],
    ["cleared", "Cache cleared"],
    ["bad_bitrate", "Invalid audio quality"],
    ["bad_togematch_api", "The togematch-api address must start with http:// or https://"],
    ["save_failed", "Couldn't save the settings"],
  ]);
  const notice = NOTICES.get(new URLSearchParams(window.location.search).get("n"));
  if (notice) alert(notice);
  if (window.location.search) {
    window.history.replaceState({}, document.title, window.location.pathname);
  }

  const bitrateSelect = document.getElementById("audio_bitrate");
  const lowDataGroup = document.getElementById("low-data-group");
  if (bitrateSelect && lowDataGroup) {
    const syncLowDataVisibility = () => {
      lowDataGroup.hidden = bitrateSelect.value !== "192k" && bitrateSelect.value !== "256k";
    };
    bitrateSelect.addEventListener("change", syncLowDataVisibility);
    syncLowDataVisibility();
  }

  // Settings save as they change, like the player switch: no Save button to miss.
  const settingsForm = document.getElementById("settings-form");
  if (settingsForm) {
    const save = async (field) => {
      if (!settingsForm.reportValidity()) return;
      let code;
      try {
        const res = await fetch(settingsForm.action, {
          method: "POST",
          body: new URLSearchParams(new FormData(settingsForm)),
        });
        code = new URL(res.url).searchParams.get("n");
      } catch {
        code = "save_failed";
      }
      if (code !== "saved") {
        alert(NOTICES.get(code) || NOTICES.get("save_failed"));
        return;
      }
      const heading = field && field.closest(".section")?.querySelector("h2");
      if (heading) {
        heading.querySelector(".saved")?.remove();
        const note = document.createElement("span");
        note.className = "saved";
        note.textContent = "Saved";
        heading.append(note);
        setTimeout(() => note.remove(), 1500);
      }
    };
    settingsForm.addEventListener("change", (e) => save(e.target));
    settingsForm.addEventListener("submit", (e) => {
      e.preventDefault();
      save(document.activeElement);
    });
  }

  const clearForm = document.getElementById("cache-clear-form");
  if (clearForm) {
    clearForm.addEventListener("submit", (e) => {
      if (!confirm("Delete all cached tracks?")) e.preventDefault();
    });
  }

  const disconnectForm = document.getElementById("disconnect-form");
  if (disconnectForm) {
    disconnectForm.addEventListener("submit", (e) => {
      if (!confirm("Disconnect togewire from your Spotify account?")) e.preventDefault();
    });
  }

  const startPairBtn = document.getElementById("start-pair-btn");
  if (startPairBtn) {
    startPairBtn.addEventListener("click", async () => {
      startPairBtn.disabled = true;
      startPairBtn.textContent = "Opening Spotify...";

      // Open new tab early in user gesture context to avoid popup blocker
      const pairWindow = window.open("about:blank", "_blank");
      if (pairWindow) pairWindow.opener = null;

      try {
        const res = await fetch("/admin/pair/start", { method: "POST" });
        if (!res.ok) {
          const errData = await res.json().catch(() => ({}));
          throw new Error(errData.error || "Failed to start pairing");
        }
        const data = await res.json();
        const linkUrl = data.verification_uri_complete ||
          `${data.verification_uri || "https://spotify.com/pair"}?code=${encodeURIComponent(data.user_code)}`;

        if (pairWindow) {
          pairWindow.location.href = linkUrl;
        } else {
          window.open(linkUrl, "_blank");
        }

        startPairBtn.textContent = "Waiting for Spotify confirmation...";

        let intervalMs = Math.max(data.interval || 5, 3) * 1000;
        const deviceCode = data.device_code;

        async function poll() {
          try {
            const pollRes = await fetch("/admin/pair/poll", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({ device_code: deviceCode }),
            });
            if (!pollRes.ok) throw new Error("Poll request failed");
            const pollData = await pollRes.json();
            if (pollData.status === "paired") {
              window.location.href = "/admin?n=paired";
              return;
            }
            if (pollData.status === "error") {
              alert("Pairing error: " + (pollData.message || "Unknown error"));
              startPairBtn.disabled = false;
              startPairBtn.textContent = "Pair with Spotify";
              return;
            }
            // RFC 8628: slow_down means add 5s to the interval from now on.
            if (pollData.status === "slow_down") intervalMs += 5000;
            setTimeout(poll, intervalMs);
          } catch {
            setTimeout(poll, intervalMs);
          }
        }

        setTimeout(poll, intervalMs);
      } catch (e) {
        if (pairWindow) pairWindow.close();
        alert("Could not start pairing: " + e.message);
        startPairBtn.disabled = false;
        startPairBtn.textContent = "Pair with Spotify";
      }
    });
  }

  const toggle = document.getElementById("player-toggle");
  const playerNav = document.getElementById("player-nav");
  const switchLabel = document.getElementById("player-toggle-label");
  if (!toggle || !playerNav) return;

  // Live status is left to pollLive.
  function setPlayerUi(enabled) {
    toggle.checked = enabled;
    playerNav.classList.toggle("is-on", enabled);
    playerNav.classList.remove("is-live");
    if (enabled) playerNav.removeAttribute("title");
    else playerNav.setAttribute("title", "Turn the player on first");
    if (switchLabel) switchLabel.textContent = enabled ? "On" : "Off";
  }

  setPlayerUi(toggle.checked);

  let busy = false;
  toggle.addEventListener("change", async () => {
    if (busy) return;
    busy = true;
    const enabled = toggle.checked;
    setPlayerUi(enabled);
    try {
      const res = await fetch("/admin/player", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled }),
      });
      if (!res.ok) throw new Error("save failed");
      const data = await res.json();
      setPlayerUi(!!data.enabled);
    } catch {
      setPlayerUi(!enabled);
      alert("Couldn't turn the player " + (enabled ? "on" : "off"));
    } finally {
      busy = false;
    }
  });

  async function pollLive() {
    if (!toggle.checked) {
      playerNav.classList.remove("is-live");
      return;
    }
    try {
      const res = await fetch("/ping");
      if (!res.ok) return;
      const data = await res.json();
      playerNav.classList.toggle("is-live", !!data.is_playing);
    } catch {
      /* ignore */
    }
  }
  setInterval(pollLive, 4000);
  pollLive();
})();
