(() => {
  const bridge = window.codexProxyPlugin;
  const $ = (id) => document.getElementById(id);
  const state = { sessionId: null, projects: [], connected: false, disconnectArmed: false };

  async function request(method, path, body) {
    const response = await bridge.request({
      method,
      path: path.replace(/^\//, ""),
      ...(body === undefined ? {} : { contentType: "application/json", body: JSON.stringify(body) }),
    });
    const text = new TextDecoder().decode(response.body);
    let data = {};
    try { data = text ? JSON.parse(text) : {}; } catch { throw new Error("Invalid response from Vertex AI plugin."); }
    if (response.status < 200 || response.status >= 300 || data.ok === false) {
      throw new Error(data.error || `Request failed (${response.status}).`);
    }
    return data;
  }

  function notice(message, kind = "") {
    const el = $("notice");
    el.textContent = message;
    el.className = `notice ${kind}`.trim();
  }

  async function copyUrl(inputId, buttonId, label) {
    const input = $(inputId);
    input.focus();
    input.select();
    try {
      await navigator.clipboard.writeText(input.value);
      notice(`${label} copied. Open it in a separate browser tab.`, "success");
    } catch {
      notice(`Select the URL in the text field and copy it manually, then open it in a separate browser tab.`);
    }
  }

  function setState(id, label, kind = "") {
    const el = $(id);
    el.textContent = label;
    el.className = `state ${kind}`.trim();
  }

  function renderProjects(projects) {
    state.projects = Array.isArray(projects) ? projects : [];
    const select = $("project");
    const selected = select.value;
    select.replaceChildren();
    if (!state.projects.length) {
      const option = document.createElement("option");
      option.value = "";
      option.textContent = "No accessible projects found";
      select.append(option);
      select.disabled = true;
      $("select-project").disabled = true;
      return;
    }
    for (const project of state.projects) {
      const option = document.createElement("option");
      option.value = project.projectId;
      option.textContent = `${project.name} (${project.projectId})`;
      select.append(option);
    }
    if (state.projects.some((project) => project.projectId === selected)) select.value = selected;
    select.disabled = false;
    $("select-project").disabled = false;
  }

  function renderStatus(data) {
    state.connected = data.connected === true;
    $("gcloud-note").textContent = data.gcloudInstalled
      ? "Google Cloud CLI is installed in the persistent CPR data volume."
      : "Google Cloud CLI is not installed in the CPR runtime yet.";
    setState("account-state", state.connected ? "Google connected" : "Not connected", state.connected ? "good" : "");
    $("connect").disabled = !data.gcloudInstalled || state.connected;
    $("disconnect").disabled = !state.connected;
    state.disconnectArmed = false;
    $("disconnect").textContent = "Disconnect Google account";
    $("identity").classList.toggle("hidden", !state.connected);
    $("identity").textContent = data.email ? `Connected as ${data.email}` : (state.connected ? "Google account connected" : "");
    $("login").classList.add("hidden");

    if (data.project) {
      setState("project-state", data.project.projectId, "good");
      $("project-info").textContent = `Location: ${data.project.location}. Billing status: ${data.project.billingEnabled === true ? "enabled" : data.project.billingEnabled === false ? "not enabled" : "not verified"}.`;
      $("billing-url").value = `https://console.cloud.google.com/billing?project=${encodeURIComponent(data.project.projectId)}`;
      setState("routing-state", data.accountReady ? "Account ready" : "Account pending", data.accountReady ? "good" : "bad");
      $("account-info").textContent = `Managed account group: ${data.accountGroup || "Vertex AI"}. Create a CPR Client Key and bind it to this group.`;
      renderProjects(data.projects || []);
      if (!state.projects.some((project) => project.projectId === data.project.projectId)) {
        const option = document.createElement("option");
        option.value = data.project.projectId;
        option.textContent = `${data.project.projectId} (current selection)`;
        $("project").append(option);
        $("project").disabled = true;
      } else {
        $("project").value = data.project.projectId;
      }
      $("select-project").disabled = $("project").disabled || !state.projects.length;
      $("select-project").textContent = "Verify and use selected project";
    } else {
      setState("project-state", "Choose a project");
      $("project-info").textContent = data.projectError || "Select the project that owns your Google Cloud billing and credits.";
      setState("routing-state", "Not configured");
      $("account-info").textContent = "The Vertex account will be created after project verification.";
      renderProjects(data.projects || []);
    }
    if (data.projectError) notice(data.projectError, "error");
    else if (state.connected) notice("Google ADC is ready. Choose a project or use the configured Vertex AI project.", "success");
    else if (!data.gcloudInstalled) notice("Google Cloud CLI is missing from the CPR runtime. Install the bundled Cloud SDK before connecting.", "error");
    else notice("Connect your Google account to continue.");
  }

  async function refresh() {
    const data = await request("GET", "api/status");
    renderStatus(data);
  }

  $("copy-auth-url").addEventListener("click", () => copyUrl("auth-url", "copy-auth-url", "Google authorization URL"));
  $("copy-billing-url").addEventListener("click", () => copyUrl("billing-url", "copy-billing-url", "Cloud Billing URL"));

  $("connect").addEventListener("click", async () => {
    try {
      notice("Starting Google sign-in…");
      const data = await request("POST", "api/login/start", {});
      state.sessionId = data.sessionId;
      $("auth-url").value = data.authorizationUrl;
      $("login").classList.remove("hidden");
      $("code").value = "";
      notice("Open the Google authorization link, approve access, and paste the one-time code.", "success");
    } catch (error) { notice(error.message, "error"); }
  });

  $("complete").addEventListener("click", async () => {
    const code = $("code").value.trim();
    if (!state.sessionId || !code) { notice("Paste the authorization code from Google.", "error"); return; }
    try {
      $("complete").disabled = true;
      notice("Completing Google authorization…");
      const data = await request("POST", "api/login/complete", { sessionId: state.sessionId, authorizationCode: code });
      state.sessionId = null;
      $("code").value = "";
      $("login").classList.add("hidden");
      renderProjects(data.projects || []);
      $("identity").classList.remove("hidden");
      $("identity").textContent = `Connected as ${data.email}`;
      $("account-state").textContent = "Google connected";
      setState("account-state", "Google connected", "good");
      $("connect").disabled = true;
      $("disconnect").disabled = false;
      notice(data.projectError || "Google account connected. Select the project to use with Vertex AI.", data.projectError ? "error" : "success");
    } catch (error) { notice(error.message, "error"); }
    finally { $("complete").disabled = false; }
  });

  $("cancel").addEventListener("click", async () => {
    try {
      await request("POST", "api/login/cancel", { sessionId: state.sessionId });
      state.sessionId = null;
      $("login").classList.add("hidden");
      notice("Google sign-in cancelled.");
    } catch (error) { notice(error.message, "error"); }
  });

  $("select-project").addEventListener("click", async () => {
    const projectId = $("project").value;
    const location = $("location").value;
    if (!projectId) { notice("Select a Google Cloud project first.", "error"); return; }
    try {
      $("select-project").disabled = true;
      notice("Checking Vertex AI API and Billing…");
      await request("POST", "api/project/select", { projectId, location });
      await refresh();
      notice("Vertex AI project connected. Bind the Vertex AI group in CPR and create a Client Key.", "success");
    } catch (error) { notice(error.message, "error"); }
    finally { $("select-project").disabled = false; }
  });

  $("disconnect").addEventListener("click", async () => {
    if (!state.disconnectArmed) {
      state.disconnectArmed = true;
      $("disconnect").textContent = "Confirm disconnect";
      notice("Click Confirm disconnect once more to remove this adapter's Google ADC credentials.");
      return;
    }
    state.disconnectArmed = false;
    try {
      $("disconnect").disabled = true;
      notice("Disconnecting Google account…");
      await request("POST", "api/disconnect", {});
      await refresh();
      notice("Google account disconnected.", "success");
    } catch (error) { notice(error.message, "error"); $("disconnect").disabled = false; }
  });

  refresh().catch((error) => notice(error.message, "error"));
})();
