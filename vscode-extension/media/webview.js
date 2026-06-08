  window.onerror = function(msg, src, line, col, err) {
    try {
      var api = acquireVsCodeApi();
      api.postMessage({ type: 'webview-error', message: msg, line: line, col: col });
    } catch(e) {}
  };
  (function() {
    'use strict';
    const vscode = acquireVsCodeApi();

    /* ── initial state ── */
    const INIT = window.__HIMALAYA_BOOTSTRAP__.INIT;
    const HISTORY = window.__HIMALAYA_BOOTSTRAP__.HISTORY;
    const LOCAL_MODELS = window.__HIMALAYA_BOOTSTRAP__.LOCAL_MODELS;

    const state = {
      model: INIT.model,
      modelBackend: INIT.modelBackend,
      permissionMode: INIT.permissionMode,
	      resumeTarget: INIT.resumeTarget || '',
	      isTrusted: INIT.isTrusted,
	      activeRecordId: INIT.activeRecordId,
	      showReasoning: INIT.showReasoning || false,
	      historyOpen: false,
      streaming: false,
      lastRunFailed: false,
      messages: [],   /* {role, text} */
      historyRecords: HISTORY
    };

    /* ── DOM refs ── */
    const thread       = document.getElementById('thread');
    const emptyState   = document.getElementById('emptyState');
    const promptInput  = document.getElementById('promptInput');
    const sendBtn      = document.getElementById('sendBtn');
    const stopBtn      = document.getElementById('stopBtn');
    const attachBtn    = document.getElementById('attachBtn');
    const attachChips  = document.getElementById('attachChips');
    const modelLabel   = document.getElementById('modelLabel');
    const modelDot     = document.getElementById('modelDot');
    const permLabel    = document.getElementById('permLabel');
    const statusDot    = document.getElementById('statusDot');
    const statusText   = document.getElementById('statusText');
	    const historyDrawer= document.getElementById('historyDrawer');
	    const historyList  = document.getElementById('historyList');
	    const trustBanner  = document.getElementById('trustBanner');

    /* ── attachment state ── */
    let attachedFiles = [];

    function renderAttachChips() {
      if (attachedFiles.length === 0) {
        attachChips.style.display = 'none';
        attachChips.innerHTML = '';
        return;
      }
      attachChips.style.display = 'flex';
      attachChips.innerHTML = attachedFiles.map((f, i) => {
        const name = f.split(/[\\/]/).pop() || f;
        return '<span class="attach-chip" title="' + esc(f) + '">'
          + '📄 ' + esc(name)
          + '<span class="remove-chip" data-idx="' + i + '">✕</span>'
          + '</span>';
      }).join('');
      attachChips.querySelectorAll('.remove-chip').forEach(el => {
        el.addEventListener('click', () => {
          attachedFiles.splice(Number(el.dataset.idx), 1);
          renderAttachChips();
        });
      });
    }

    if (attachBtn) {
      attachBtn.addEventListener('click', () => {
        try { vscode.postMessage({ type: 'pick-file' }); } catch (_) {}
      });
    }

    if (stopBtn) {
      stopBtn.addEventListener('click', () => {
        try {
          vscode.postMessage({ type: 'cancel' });
          state.streaming = false;
          updateSendButtonState();
          setStatus('Cancelled', '');
          if (streamCursor && streamCursor.remove) { streamCursor.remove(); }
          streamCursor = null;
          streamBuffer = '';
          streamBubble = null;
        } catch (_) {}
      });
    }

    /* ── helpers ── */
    function esc(s) {
      return String(s ?? '')
        .replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')
        .replace(/"/g,'&quot;').replace(/'/g,'&#39;');
    }

    function copyToClipboard(text) {
      try {
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(text);
        } else {
          var ta = document.createElement('textarea');
          ta.value = text;
          ta.style.position = 'fixed';
          ta.style.left = '-9999px';
          document.body.appendChild(ta);
          ta.select();
          document.execCommand('copy');
          document.body.removeChild(ta);
        }
      } catch (_) {}
    }

    function addCopyButton(el, text) {
      var btn = document.createElement('button');
      btn.className = 'msg-action-btn';
      btn.textContent = '📋 Copy';
      btn.addEventListener('click', function(e) {
        e.stopPropagation();
        copyToClipboard(text);
        btn.textContent = '✓ Copied';
        btn.classList.add('copied');
        setTimeout(function() { btn.textContent = '📋 Copy'; btn.classList.remove('copied'); }, 2000);
      });
      el.appendChild(btn);
    }

    function injectCopyCodeButtons(bodyEl) {
      if (!bodyEl) { return; }
      var pres = bodyEl.querySelectorAll('pre');
      pres.forEach(function(pre) {
        if (pre.querySelector('.copy-code-btn')) { return; }
        var wrapper = document.createElement('div');
        wrapper.className = 'code-block-wrapper';
        pre.parentNode.insertBefore(wrapper, pre);
        wrapper.appendChild(pre);
        var btn = document.createElement('button');
        btn.className = 'copy-code-btn';
        btn.textContent = '📋 Copy';
        btn.addEventListener('click', function() {
          var code = pre.textContent || '';
          copyToClipboard(code);
          btn.textContent = '✓ Copied';
          setTimeout(function() { btn.textContent = '📋 Copy'; }, 2000);
        });
        wrapper.appendChild(btn);
      });
    }

    function addActionsBar(div, text) {
      var actions = document.createElement('div');
      actions.className = 'msg-actions';
      addCopyButton(actions, text);
      div.appendChild(actions);
    }

    function setStatus(text, kind) {
      statusText.textContent = text;
      statusDot.className = 'status-dot' + (kind ? ' ' + kind : '');
    }

    function updateSendButtonState() {
      try {
        if (!sendBtn) { return; }
        const blocked = !state.isTrusted;
        sendBtn.disabled = state.streaming || blocked;
        sendBtn.style.display = state.streaming ? 'none' : '';
        if (stopBtn) { stopBtn.style.display = state.streaming ? '' : 'none'; }
        sendBtn.title = blocked
          ? 'Trust the workspace or enable himalayaCode.allowUntrustedRuns to run prompts'
          : state.streaming
            ? 'A request is already running'
            : 'Send prompt';
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateSendButtonState failed: ' + String(e) }); } catch (_) {}
      }
    }

    function updateModelBar() {
      try {
        if (!modelLabel || !modelDot || !permLabel) { return; }
        const b = state.modelBackend;
        const dotClass = b === 'cloud' ? 'cloud' : b === 'ollama' ? 'local' : 'unknown';
        modelDot.className = 'dot ' + dotClass;
        modelLabel.textContent = state.model || 'No model';
        permLabel.textContent = state.permissionMode || 'read-only';
        // Update perm pill color class
        if (permLabel) {
          permLabel.classList.remove('read-only', 'workspace-write', 'danger-full-access');
          var pm = (state.permissionMode || '').toLowerCase().replace(/_/g, '-');
          if (pm === 'read-only' || pm === 'workspace-write' || pm === 'danger-full-access') {
            permLabel.classList.add(pm);
          }
        }
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateModelBar failed: ' + String(e) }); } catch (_) {}
      }
    }

    function autoResize() {
      promptInput.style.height = 'auto';
      promptInput.style.height = Math.min(promptInput.scrollHeight, 160) + 'px';
    }

    function scrollBottom() {
      thread.scrollTop = thread.scrollHeight;
    }

    function showEmpty(show) {
      emptyState.style.display = show ? 'flex' : 'none';
    }

    /* ── message rendering ── */
    let streamBubble = null;
    let streamCursor = null;
    let streamBuffer = '';

    /* marked.js configuration */
    if (typeof marked !== 'undefined' && marked.setOptions) {
      marked.setOptions({ gfm: true, breaks: false });
    }

    function renderMarkdown(md) {
      try {
        if (typeof marked !== 'undefined') {
          return marked.parse(md, { mangle: false, headerIds: false });
        }
        return esc(md);
      } catch (e) {
        return esc(md);
      }
    }

    function startStream() {
      try {
        if (!thread) { return; }
        showEmpty(false);
        streamBuffer = '';
        streamBubble = document.createElement('div');
        streamBubble.className = 'msg assistant';
        streamBubble.innerHTML = '<div class="msg-role">Himalaya</div><div class="msg-body"></div>';
        thread.appendChild(streamBubble);
        streamCursor = document.createElement('span');
        streamCursor.className = 'cursor';
        const body = streamBubble.querySelector('.msg-body');
        if (body) { body.appendChild(streamCursor); }
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'startStream failed: ' + String(e) }); } catch (_) {}
      }
    }

    function appendStream(text) {
      try {
        if (!streamBubble) { startStream(); }
        const body = streamBubble && streamBubble.querySelector ? streamBubble.querySelector('.msg-body') : null;
        if (!body) { return; }
        streamBuffer += text;
        // Incremental markdown render: keep cursor at end
        if (streamCursor && streamCursor.parentNode === body) {
          body.removeChild(streamCursor);
        }
        body.innerHTML = renderMarkdown(streamBuffer);
        if (streamCursor) {
          body.appendChild(streamCursor);
        }
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'appendStream failed: ' + String(e) }); } catch (_) {}
      }
    }

    function endStream() {
      try {
        if (streamCursor && streamCursor.remove) { streamCursor.remove(); }
        streamCursor = null;
        // Final render of accumulated text
        if (streamBubble) {
          const body = streamBubble.querySelector('.msg-body');
          if (body && streamBuffer) {
            body.innerHTML = renderMarkdown(streamBuffer);
            injectCopyCodeButtons(body);
          }
          addActionsBar(streamBubble, streamBuffer);
        }
        streamBuffer = '';
        streamBubble = null;
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'endStream failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── history drawer ── */
    function renderHistory() {
      historyList.innerHTML = '';
      if (state.historyRecords.length === 0) {
        historyList.innerHTML = '<div style="padding:10px;color:var(--text-dim);font-size:12px;">No history yet.</div>';
        return;
      }
      state.historyRecords.forEach(function(rec) {
        const item = document.createElement('div');
        item.className = 'history-item' + (rec.id === state.activeRecordId ? ' active' : '');
	        const date = new Date(rec.updatedAt || rec.createdAt || 0).toLocaleDateString();
	        item.innerHTML =
	          '<span class="hi-title">' + esc(rec.title || 'Untitled') + '</span>' +
	          '<span class="hi-meta">' + esc(date) + '</span>' +
	          '<button type="button" class="history-delete" title="Delete history" aria-label="Delete history">&#128465;</button>';
	        const deleteButton = item.querySelector('.history-delete');
	        if (deleteButton) {
	          deleteButton.addEventListener('click', function(event) {
	            event.stopPropagation();
	            if (!window.confirm('Delete this history record?')) { return; }
	            state.historyRecords = state.historyRecords.filter(function(item) { return item.id !== rec.id; });
	            if (state.activeRecordId === rec.id) {
	              state.activeRecordId = null;
	              state.resumeTarget = '';
	              state.messages = [];
	              renderThread();
	            }
	            renderHistory();
	            setStatus('History deleted.', 'done');
	            vscode.postMessage({ type: 'history-action', action: 'delete', historyId: rec.id });
	          });
	        }
	        item.addEventListener('click', function() {
	          state.activeRecordId = rec.id;
          vscode.postMessage({ type: 'history-action', historyId: rec.id, selectedHistoryId: rec.id });
          /* load messages from record */
          state.messages = (rec.messages || []).map(function(m) { return { role: m.role, text: m.text }; });
          renderThread();
          renderHistory();
          toggleHistory(false);
        });
        historyList.appendChild(item);
      });
    }

    function toggleHistory(force) {
      state.historyOpen = force !== undefined ? force : !state.historyOpen;
      historyDrawer.classList.toggle('open', state.historyOpen);
    }

    function addBubble(role, text) {
      try {
        if (!thread) { return; }
        const div = document.createElement('div');
        const cls = role === 'user' ? 'msg user' : role === 'assistant' ? 'msg assistant' : role === 'error' ? 'msg error' : role === 'stderr' ? 'msg stderr' : role === 'tool-step' ? 'msg tool-step' : 'msg';
        div.className = cls;
        const label = role === 'user' ? 'You' : role === 'assistant' ? 'Himalaya' : role === 'tool-step' ? 'Tool' : (String(role || '')[0] || '').toUpperCase() + String(role || '').slice(1);
        // Render markdown for assistant messages
        const bodyContent = role === 'assistant' ? renderMarkdown(text || '') : esc(text || '');
        div.innerHTML = '<div class="msg-role">' + esc(label) + '</div><div class="msg-body">' + bodyContent + '</div>';
        // Add copy buttons for assistant and user messages
        if (role === 'assistant') {
          addActionsBar(div, text || '');
          var bodyEl = div.querySelector('.msg-body');
          if (bodyEl) { injectCopyCodeButtons(bodyEl); }
        }
        if (role === 'user') {
          addActionsBar(div, text || '');
        }
        thread.appendChild(div);
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addBubble failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── tool step rendering ── */
    // Map tool names to icons and categories
    function getToolMeta(name) {
      var n = String(name || '').toLowerCase();
      if (n === 'bash' || n.includes('bash')) return { icon: '⚡', color: '#e6b422', cat: 'bash' };
      if (n.includes('read') || n.includes('view')) return { icon: '📖', color: '#4fc1ff', cat: 'read' };
      if (n.includes('write') || n.includes('create')) return { icon: '✏️', color: '#4ec9b0', cat: 'write' };
      if (n.includes('edit') || n.includes('replace')) return { icon: '✂️', color: '#ffa726', cat: 'edit' };
      if (n.includes('grep') || n.includes('search') || n.includes('glob') || n.includes('find')) return { icon: '🔍', color: '#9b59b6', cat: 'search' };
      if (n.includes('web') || n.includes('fetch') || n.includes('url')) return { icon: '🌐', color: '#3498db', cat: 'web' };
      if (n.includes('agent') || n.includes('task')) return { icon: '🤖', color: '#e67e22', cat: 'agent' };
      if (n.includes('memory') || n.includes('remember')) return { icon: '🧠', color: '#1abc9c', cat: 'memory' };
      if (n.includes('todo') || n.includes('plan')) return { icon: '📋', color: '#f39c12', cat: 'planning' };
      if (n.includes('mcp')) return { icon: '🔌', color: '#8e44ad', cat: 'mcp' };
      return { icon: '🔧', color: '#95a5a6', cat: 'other' };
    }

    function addToolStep(msg) {
      try {
        if (!thread) { return; }
        var meta = getToolMeta(msg.name);
        var cardId = 'ts-' + Date.now() + '-' + Math.random().toString(36).slice(2,8);

        if (msg.step === 'use') {
          // Tool use: show as collapsible card
          var inputPreview = String(msg.input || '{}');
          try { var parsed = JSON.parse(inputPreview); inputPreview = JSON.stringify(parsed, null, 2); } catch(_) {}
          var div = document.createElement('div');
          div.className = 'msg tool-step tool-use';
          div.innerHTML =
            '<div class="msg-role tool-toggle" data-card="' + cardId + '" style="color:' + meta.color + '">' +
              '<span class="tool-toggle-arrow" id="arrow-' + cardId + '">▶</span> ' +
              meta.icon + ' <strong>' + esc(msg.name || 'tool') + '</strong>' +
              '<span class="tool-cat-chip">' + meta.cat + '</span>' +
            '</div>' +
            '<div class="msg-body tool-card" id="' + cardId + '" style="display:none; border-left-color:' + meta.color + '">' +
              '<pre class="tool-input">' + esc(inputPreview) + '</pre>' +
            '</div>';
          thread.appendChild(div);
          var toggle = div.querySelector('.tool-toggle');
          if (toggle) {
            toggle.addEventListener('click', function() {
              var card = document.getElementById(cardId);
              var arrow = document.getElementById('arrow-' + cardId);
              if (card && arrow) {
                var isOpen = card.style.display !== 'none';
                card.style.display = isOpen ? 'none' : 'block';
                arrow.textContent = isOpen ? '▶' : '▼';
              }
            });
          }
        } else {
          // Tool result: show with output
          var output = String(msg.output || '');
          var isError = Boolean(msg.isError);
          var div = document.createElement('div');
          div.className = 'msg tool-step tool-result' + (isError ? ' tool-error' : '');
          var label = isError ? 'Error' : 'Result';
          var labelColor = isError ? 'var(--danger)' : meta.color;
          div.innerHTML =
            '<div class="msg-role" style="color:' + labelColor + '">' +
              meta.icon + ' <strong>' + esc(msg.name || 'tool') + '</strong> → ' + label +
              '<span class="tool-cat-chip">' + meta.cat + '</span>' +
            '</div>' +
            '<div class="msg-body tool-output-body">' +
              renderMarkdown(output) +
            '</div>';
          var toolBody = div.querySelector('.tool-output-body');
          if (toolBody) { injectCopyCodeButtons(toolBody); }
          thread.appendChild(div);
        }
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addToolStep failed: ' + String(e) }); } catch (_) {}
      }
    }

    /* ── reasoning visualization ── */
    function addReasoningStep(step) {
      try {
        if (!step) { return; }
        if (!state.showReasoning) { return; }
        if (!thread) { return; }
        const stepType = String(step.step_type || 'reason').toLowerCase();
        const div = document.createElement('div');
        div.className = 'msg reasoning-step';

        var icon, label, bodyHtml;
        switch (stepType) {
          case 'analysis':
            icon = '🔍'; label = 'Analysis';
            bodyHtml = renderMarkdown(String(step.content || ''));
            if (step.confidence != null) {
              bodyHtml += '<span class="reasoning-confidence">Confidence: ' + Math.round(step.confidence * 100) + '%</span>';
            }
            break;
          case 'planning':
            icon = '📋'; label = 'Plan';
            bodyHtml = renderMarkdown(String(step.plan || ''));
            if (Array.isArray(step.steps) && step.steps.length > 0) {
              bodyHtml += '<ol class="reasoning-plan-steps">';
              step.steps.forEach(function(s) {
                bodyHtml += '<li>' + esc(String(s)) + '</li>';
              });
              bodyHtml += '</ol>';
            }
            break;
          case 'reflection':
            icon = '💭'; label = 'Reflection';
            bodyHtml = '<div class="reasoning-critique">' + renderMarkdown(String(step.critique || '')) + '</div>';
            if (step.adjustment) {
              bodyHtml += '<div class="reasoning-adjustment"><strong>Strategy adjustment:</strong> ' + renderMarkdown(String(step.adjustment)) + '</div>';
            }
            break;
          case 'decision':
            icon = '✅'; label = 'Decision';
            bodyHtml = '<strong>Choice:</strong> ' + esc(String(step.choice || '')) + '<br>' + renderMarkdown(String(step.reasoning || ''));
            break;
          default:
            icon = '🧠'; label = stepType;
            bodyHtml = renderMarkdown(String(step.content || JSON.stringify(step)));
        }

        // Collapsible card
        var cardId = 'rs-' + Date.now() + '-' + Math.random().toString(36).slice(2,8);
        div.innerHTML =
          '<div class="msg-role reasoning-toggle" data-card="' + cardId + '">' +
            '<span class="reasoning-toggle-arrow" id="arrow-' + cardId + '">▶</span> ' +
            icon + ' ' + esc(label) +
          '</div>' +
          '<div class="msg-body reasoning-card" id="' + cardId + '" style="display:none">' +
            bodyHtml +
          '</div>';
        thread.appendChild(div);

        // Click handler for toggle
        var toggle = div.querySelector('.reasoning-toggle');
        if (toggle) {
          toggle.addEventListener('click', function() {
            var card = document.getElementById(cardId);
            var arrow = document.getElementById('arrow-' + cardId);
            if (card && arrow) {
              var isOpen = card.style.display !== 'none';
              card.style.display = isOpen ? 'none' : 'block';
              arrow.textContent = isOpen ? '▶' : '▼';
            }
          });
        }
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addReasoningStep failed: ' + String(e) }); } catch (_) {}
      }
    }

    function normalizeDecisioningRiskLevel(event) {
      const explicit = String(event && event.risk_level ? event.risk_level : '').toLowerCase();
      if (explicit === 'low' || explicit === 'medium' || explicit === 'high') {
        return explicit;
      }
      const action = String(event && event.action ? event.action : '').toLowerCase();
      if (action === 'deny') { return 'high'; }
      if (action === 'review') { return 'medium'; }
      if (action === 'allow') { return 'low'; }
      const score = typeof event.risk_score === 'number' ? event.risk_score : undefined;
      if (typeof score === 'number') {
        if (score >= 0.7) { return 'high'; }
        if (score >= 0.35) { return 'medium'; }
        return 'low';
      }
      return 'unknown';
    }

    function renderDecisioningSummaryBadge(label, className) {
      return '<span class="decisioning-badge' + (className ? ' ' + className : '') + '">' + esc(label) + '</span>';
    }

	    function renderDecisioningEventMarkup(event) {
      const kind = esc(renderDecisioningKindLabel(event.kind));
      const title = esc(String(event.title || 'Decisioning'));
      const summary = String(event.summary || '');
      const riskLevel = normalizeDecisioningRiskLevel(event);
      const badges = [];
      if (event.task_id) { badges.push(renderDecisioningSummaryBadge('task ' + String(event.task_id))); }
      if (typeof event.confidence === 'number') { badges.push(renderDecisioningSummaryBadge('confidence ' + Math.round(event.confidence * 100) + '%')); }
      if (typeof event.risk_score === 'number') { badges.push(renderDecisioningSummaryBadge('risk ' + Math.round(event.risk_score * 100) + '%', 'risk-' + riskLevel)); }
      if (typeof event.parallelizable === 'boolean') { badges.push(renderDecisioningSummaryBadge(event.parallelizable ? 'parallel' : 'serial')); }
      if (event.action) { badges.push(renderDecisioningSummaryBadge(String(event.action), 'action-' + String(event.action).toLowerCase())); }
      if (Array.isArray(event.selected_tools) && event.selected_tools.length > 0) { badges.push(renderDecisioningSummaryBadge(event.selected_tools.length + ' tool(s)')); }
      if (typeof event.risk_score === 'number' || event.risk_level || event.action) {
        badges.push(renderDecisioningSummaryBadge(riskLevel + ' risk', 'risk-' + riskLevel));
      }
      const sections = [];
      const riskHtml = renderDecisioningRiskPanel(event, riskLevel);
      if (riskHtml) { sections.push(riskHtml); }
      const toolScoresHtml = renderDecisioningToolScores(event.tool_scores, event.selected_tools);
      if (toolScoresHtml) { sections.push(toolScoresHtml); }
      const planTreeHtml = renderDecisioningPlanTree(event.plan_tree, event.selected_tools);
      if (planTreeHtml) { sections.push(planTreeHtml); }
      const notesHtml = renderDecisioningNotes(event.details);
      if (notesHtml) { sections.push(notesHtml); }
      const body = summary ? '<div class="decisioning-summary">' + esc(summary) + '</div>' : '<div class="decisioning-summary">' + esc(JSON.stringify(event, null, 2)) + '</div>';
      return '<div class="msg-role">Decisioning · ' + kind + ' · ' + title + '</div><div class="msg-body"><div class="decisioning-card"><div class="decisioning-header">' + body + '<div class="decisioning-badges">' + badges.join('') + '</div></div>' + sections.join('') + '</div></div>';
    }

	    function renderDecisioningKindLabel(kind) {
      const raw = String(kind || '').trim();
      const lookup = {
        tool_selection: 'Tool selection',
        task_decomposition: 'Task decomposition',
        parallelism_decision: 'Parallelism decision',
        safety_assessment: 'Safety assessment',
        plan_adjustment: 'Plan adjustment'
      };
      if (lookup[raw]) {
        return lookup[raw];
      }
      if (!raw) {
        return 'Decision event';
      }
      return raw
        .split('_')
        .map(function(part) {
          return part ? part.charAt(0).toUpperCase() + part.slice(1) : part;
        })
        .join(' ');
    }

    function renderDecisioningRiskPanel(event, riskLevel) {
      const riskScore = typeof event.risk_score === 'number' ? Math.max(0, Math.min(1, event.risk_score)) : undefined;
      const action = String(event && event.action ? event.action : '').toLowerCase();
      const hasAction = action === 'allow' || action === 'review' || action === 'deny';
      const hasRisk = typeof riskScore === 'number' || riskLevel !== 'unknown' || hasAction;
      const reasonList = Array.isArray(event && event.details) ? event.details.filter(Boolean).slice(0, 4) : [];
      if (!hasRisk && !reasonList.length) { return ''; }
      const fill = typeof riskScore === 'number'
        ? Math.round(riskScore * 100)
        : riskLevel === 'high'
          ? 88
          : riskLevel === 'medium'
            ? 55
            : riskLevel === 'low'
              ? 18
              : 0;
      const summaryBits = [];
      if (typeof riskScore === 'number') { summaryBits.push('score ' + riskScore.toFixed(2)); }
      if (hasAction) { summaryBits.push('action ' + action); }
      if (reasonList.length) { summaryBits.push(reasonList.length + ' reason(s)'); }
      const details = reasonList.length ? '<div class="decisioning-risk-details">' + reasonList.map(function(reason) {
        return '<div>• ' + esc(String(reason || '')) + '</div>';
      }).join('') + '</div>' : '';
      return '<section class="decisioning-section decisioning-risk-panel">' +
        '<div class="decisioning-section-title">Risk Grade</div>' +
        '<div class="decisioning-risk-summary">' +
          '<div class="decisioning-risk-label">' + esc(riskLevel.toUpperCase() + ' risk') + '</div>' +
          '<div class="decisioning-score-value">' + (typeof riskScore === 'number' ? esc(Math.round(riskScore * 100) + '%') : esc(riskLevel)) + '</div>' +
        '</div>' +
        '<div class="decisioning-risk-meter"><span style="width:' + fill + '%"></span></div>' +
        (summaryBits.length ? '<div class="decisioning-section-note">' + esc(summaryBits.join(' · ')) + '</div>' : '') +
        details +
      '</section>';
    }

    function renderDecisioningToolScores(toolScores, selectedTools) {
      const list = Array.isArray(toolScores) ? toolScores.filter(Boolean) : [];
      if (!list.length) { return ''; }
      const selectedSet = new Set(Array.isArray(selectedTools) ? selectedTools.map(function(name) { return String(name || ''); }) : []);
      const visibleScores = list.slice(0, 6);
      const baseline = visibleScores[0] && typeof visibleScores[0].score === 'number' ? visibleScores[0].score : 0;
      const minScore = visibleScores.reduce(function(acc, item) {
        return Math.min(acc, typeof item.score === 'number' ? item.score : 0);
      }, baseline);
      const maxScore = visibleScores.reduce(function(acc, item) {
        return Math.max(acc, typeof item.score === 'number' ? item.score : 0);
      }, baseline);
      const leaderScore = typeof maxScore === 'number' ? maxScore : 0;
      const span = maxScore - minScore;
      const items = visibleScores.map(function(item) {
        const scoreValue = typeof item.score === 'number' ? item.score : 0;
        const selected = Boolean(item.selected) || selectedSet.has(String(item.name || ''));
        const fill = span === 0 ? 100 : Math.max(0, Math.min(100, Math.round(((scoreValue - minScore) / span) * 100)));
        const meta = [];
        meta.push('#' + (visibleScores.indexOf(item) + 1));
        if (scoreValue === leaderScore) { meta.push('leader'); }
        else if (typeof leaderScore === 'number') { meta.push((leaderScore - scoreValue).toFixed(2) + ' behind leader'); }
        if (typeof item.success_rate === 'number') { meta.push(Math.round(item.success_rate * 100) + '% success'); }
        if (typeof item.latency_ms === 'number') { meta.push(item.latency_ms + ' ms'); }
        if (typeof item.cost === 'number') { meta.push('$' + item.cost.toFixed(2)); }
        meta.push(item.parallelizable ? 'parallel' : 'serial');
        const capabilities = Array.isArray(item.capabilities) ? item.capabilities.slice(0, 3) : [];
        const capabilityCount = Array.isArray(item.capabilities) ? item.capabilities.length : 0;
        return '<div class="decisioning-score-item' + (selected ? ' selected' : '') + '">' +
          '<div class="decisioning-score-header">' +
            '<div>' +
              '<div class="decisioning-score-name">' +
                esc(String(item.name || 'tool')) +
                (selected ? '<span class="decisioning-chip selected-tag">selected</span>' : '') +
              '</div>' +
              '<div class="decisioning-score-meta">' + esc(meta.join(' · ')) + '</div>' +
            '</div>' +
            '<div class="decisioning-score-value">' + esc(scoreValue.toFixed(2)) + '</div>' +
          '</div>' +
          '<div class="decisioning-score-bar"><span style="width:' + fill + '%"></span></div>' +
          (capabilities.length ? '<div class="decisioning-chip-row">' +
            capabilities.map(function(capability) { return '<span class="decisioning-chip">' + esc(String(capability || '')) + '</span>'; }).join('') +
            (capabilityCount > capabilities.length ? '<span class="decisioning-chip">+' + (capabilityCount - capabilities.length) + '</span>' : '') +
          '</div>' : '') +
        '</div>';
      }).join('');
      const footer = list.length > visibleScores.length ? '<div class="decisioning-section-note">Showing top ' + visibleScores.length + ' of ' + list.length + ' scored tools.</div>' : '';
      return '<section class="decisioning-section">' +
        '<div class="decisioning-section-title">Tool Scores</div>' +
        '<div class="decisioning-score-list">' + items + '</div>' +
        footer +
      '</section>';
    }

    function renderDecisioningPlanNode(node, selectedTools, depth) {
      if (!node) { return ''; }
      const selectedSet = selectedTools instanceof Set ? selectedTools : new Set(Array.isArray(selectedTools) ? selectedTools.map(function(name) { return String(name || ''); }) : []);
      const kind = String(node.kind || 'step').toLowerCase();
      const title = esc(String(node.title || 'Untitled plan node'));
      const id = String(node.id || '');
      const tools = Array.isArray(node.candidate_tools) ? node.candidate_tools : [];
      const notes = Array.isArray(node.notes) ? node.notes : [];
      const children = Array.isArray(node.children) ? node.children : [];
      const meta = [];
      if (typeof node.estimated_effort === 'number') { meta.push('effort ' + node.estimated_effort); }
      meta.push(node.parallelizable ? 'parallel' : 'serial');
      if (id) { meta.push(id); }
      const toolChips = tools.length ? '<div class="decisioning-chip-row">' + tools.map(function(toolName) {
        const label = String(toolName || '');
        return '<span class="decisioning-chip' + (selectedSet.has(label) ? ' selected' : '') + '">' + esc(label) + '</span>';
      }).join('') + '</div>' : '';
      const noteBlock = notes.length ? '<div class="decisioning-node-notes">' + notes.map(function(note) {
        return '<div>' + esc(String(note || '')) + '</div>';
      }).join('') + '</div>' : '';
      const childBlock = children.length ? '<div class="decisioning-tree-children">' + children.map(function(child) {
        return renderDecisioningPlanNode(child, selectedSet, depth + 1);
      }).join('') + '</div>' : '';
      return '<div class="decisioning-tree-node kind-' + esc(kind) + ' level-' + depth + '">' +
        '<div class="decisioning-tree-head">' +
          '<div class="decisioning-tree-title">' +
            '<span class="decisioning-tree-kind">' + esc(kind) + '</span>' +
            '<span class="decisioning-tree-text">' + title + '</span>' +
          '</div>' +
          (meta.length ? '<div class="decisioning-tree-meta">' + esc(meta.join(' · ')) + '</div>' : '') +
        '</div>' +
        toolChips +
        noteBlock +
        childBlock +
      '</div>';
    }

    function renderDecisioningPlanTree(node, selectedTools) {
      if (!node) { return ''; }
      const selectedSet = new Set(Array.isArray(selectedTools) ? selectedTools.map(function(name) { return String(name || ''); }) : []);
      return '<section class="decisioning-section">' +
        '<div class="decisioning-section-title">Plan Tree</div>' +
        '<div class="decisioning-tree">' + renderDecisioningPlanNode(node, selectedSet, 0) + '</div>' +
      '</section>';
    }

    function renderDecisioningNotes(details) {
      const list = Array.isArray(details) ? details.filter(Boolean) : [];
      if (!list.length) { return ''; }
      return '<section class="decisioning-section">' +
        '<div class="decisioning-section-title">Notes</div>' +
        '<div class="decisioning-node-notes">' + list.map(function(item) {
          return '<div>' + esc(String(item || '')) + '</div>';
        }).join('') + '</div>' +
      '</section>';
    }

    function addDecisioningEvent(event) {
      try {
        if (!event) { return; }
        if (!thread) { return; }
        const div = document.createElement('div');
        div.className = 'msg decisioning-step';
        div.innerHTML = renderDecisioningEventMarkup(event);
        thread.appendChild(div);
        scrollBottom();
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'addDecisioningEvent failed: ' + String(e) }); } catch (_) {}
      }
    }

    function updateReasoningToggle() {
      try {
        const btn = document.getElementById('btnReasoning');
        if (!btn) { return; }
        btn.style.opacity = state.showReasoning ? '1' : '0.6';
        btn.title = state.showReasoning ? 'Hide reasoning visualization' : 'Show reasoning visualization';
      } catch (e) {
        try { vscode.postMessage({ type: 'webview-error', message: 'updateReasoningToggle failed: ' + String(e) }); } catch (_) {}
      }
    }



    /* ── thread rendering ── */
    function renderThread() {
      thread.innerHTML = '';
      streamBubble = null;
      streamCursor = null;
      if (state.messages.length === 0) {
        thread.appendChild(emptyState);
        showEmpty(true);
        return;
      }
      showEmpty(false);
      state.messages.forEach(function(m) { addBubble(m.role, m.text); });
    }

    /* ── submit ── */
    function submit() {
      const text = promptInput.value.trim();
      if (!text || state.streaming) { return; }
      if (!state.isTrusted) {
        showEmpty(false);
        addBubble('error', 'Prompt execution is blocked in this workspace. Trust the workspace or enable himalayaCode.allowUntrustedRuns.');
        setStatus('Workspace untrusted — execution blocked.', 'error');
        scrollBottom();
        return;
      }
      state.messages.push({ role: 'user', text });
      addBubble('user', text);
      promptInput.value = '';
      promptInput.style.height = 'auto';
      const filesToSend = attachedFiles.slice();
      attachedFiles = [];
      renderAttachChips();
      state.streaming = true;
      state.lastRunFailed = false;
      updateSendButtonState();
      setStatus('Running…', 'running');
      startStream();
      vscode.postMessage({
        type: 'submit',
        prompt: text,
        permissionMode: state.permissionMode,
        resumeTarget: state.resumeTarget,
        files: filesToSend
      });
    }

    /* ── event wiring ── */
    if (sendBtn) { sendBtn.addEventListener('click', submit); }

    if (promptInput) {
      promptInput.addEventListener('keydown', function(e) {
        try {
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            submit();
          }
        } catch (_) {}
        setTimeout(autoResize, 0);
      });
      promptInput.addEventListener('input', autoResize);
    }

    const btnModelEl = document.getElementById('btnModel');
    if (btnModelEl) {
      btnModelEl.addEventListener('click', function() {
        try { vscode.postMessage({ type: 'command', command: 'configureModel' }); } catch (_) {}
      });
    }

    const btnPermEl = document.getElementById('btnPerm');
    if (btnPermEl) {
      btnPermEl.addEventListener('click', function() {
        try {
          const modes = ['read-only', 'workspace-write', 'danger-full-access'];
          const idx = modes.indexOf(state.permissionMode);
          state.permissionMode = modes[(idx + 1) % modes.length];
          updateModelBar();
        } catch (_) {}
      });
    }

    const btnHistoryEl = document.getElementById('btnHistory');
    if (btnHistoryEl) {
      btnHistoryEl.addEventListener('click', function() {
        try { renderHistory(); toggleHistory(); } catch (_) {}
      });
    }

    const btnNewEl = document.getElementById('btnNew');
    if (btnNewEl) {
      btnNewEl.addEventListener('click', function() {
        try {
          state.messages = [];
          state.activeRecordId = null;
          state.resumeTarget = '';
          renderThread();
          setStatus('New session.', '');
          vscode.postMessage({ type: 'command', command: 'newSession' });
        } catch (_) {}
      });
    }

    const btnReasoningEl = document.getElementById('btnReasoning');
    if (btnReasoningEl) {
      btnReasoningEl.addEventListener('click', function() {
        try {
          state.showReasoning = !state.showReasoning;
          updateReasoningToggle();
          try { vscode.postMessage({ type: 'toggle-reasoning', enabled: state.showReasoning }); } catch (_) {}
        } catch (_) {}
      });
    }
	    // ensure initial visual state
	    try { updateReasoningToggle(); } catch (_) {}

    const btnRefreshEl = document.getElementById('btnRefresh');
    if (btnRefreshEl) {
      btnRefreshEl.addEventListener('click', function() {
        try { vscode.postMessage({ type: 'refresh' }); } catch (_) {}
      });
    }

    const btnDoctorEl = document.getElementById('btnDoctor');
    if (btnDoctorEl) {
      btnDoctorEl.addEventListener('click', function() { try { vscode.postMessage({ type: 'command', command: 'doctor' }); } catch (_) {} });
    }

    const btnStatusEl = document.getElementById('btnStatus');
    if (btnStatusEl) {
      btnStatusEl.addEventListener('click', function() { try { vscode.postMessage({ type: 'command', command: 'status' }); } catch (_) {} });
    }

    const quickChipsEl = document.getElementById('quickChips');
    if (quickChipsEl) {
      quickChipsEl.addEventListener('click', function(e) {
        try {
          if (!(e.target instanceof Element)) { return; }
          const chip = e.target.closest('[data-prompt]');
          if (!chip) { return; }
          promptInput.value = chip.getAttribute('data-prompt');
          promptInput.focus();
          autoResize();
        } catch (_) {}
      });
    }

    /* ── messages from extension host ── */
    window.addEventListener('message', function(event) {
      const msg = event.data;
      if (!msg || !msg.type) { return; }
      switch (msg.type) {
        case 'files-picked':
          if (Array.isArray(msg.paths)) {
            attachedFiles = attachedFiles.concat(msg.paths);
            renderAttachChips();
          }
          break;
        case 'bootstrap':
          if (msg.bootstrap) {
            state.isTrusted = Boolean(msg.bootstrap.trust);
            if (msg.bootstrap.history) {
              state.historyRecords = msg.bootstrap.history.records || [];
              state.activeRecordId = msg.bootstrap.history.activeRecordId || null;
            }
          }
          if (msg.options) {
            if (msg.options.model) { state.model = msg.options.model; }
            if (msg.options.modelBackend) { state.modelBackend = msg.options.modelBackend; }
	            if (msg.options.permissionMode) { state.permissionMode = msg.options.permissionMode; }
	            if (msg.options.resumeTarget !== undefined) { state.resumeTarget = msg.options.resumeTarget || ''; }
	            if (msg.options.showReasoning !== undefined) { state.showReasoning = Boolean(msg.options.showReasoning); }
	          }
          trustBanner.hidden = state.isTrusted;
            setStatus(
              state.isTrusted ? 'Ready' : 'Workspace untrusted — execution blocked.',
              state.isTrusted ? '' : 'error'
            );
	          updateModelBar();
	          updateSendButtonState();
	          try { updateReasoningToggle(); } catch (_) {}
	          break;
	        case 'historyDeleted':
	          if (msg.historyId) {
	            state.historyRecords = state.historyRecords.filter(function(rec) { return rec.id !== msg.historyId; });
	            if (state.activeRecordId === msg.historyId || msg.selectedHistoryId === null) {
	              state.activeRecordId = msg.activeRecordId || null;
	              if (!state.activeRecordId) {
	                state.messages = [];
	                state.resumeTarget = '';
	                renderThread();
	              }
	            }
	            renderHistory();
	            updateSendButtonState();
	            setStatus('History deleted.', 'done');
	          }
	          break;

	        case 'session-reset':
          state.messages = [];
          state.activeRecordId = null;
          state.resumeTarget = '';
          renderThread();
          setStatus('Ready', '');
          break;
        case 'model-updated':
          if (msg.model) { state.model = msg.model; }
          if (msg.modelBackend) { state.modelBackend = msg.modelBackend; }
          updateModelBar();
          setStatus('Model updated: ' + state.model, 'done');
          break;
        case 'assistantStart':
          state.lastRunFailed = false;
          state.streaming = true;
          if (!streamBubble) { startStream(); }
          updateSendButtonState();
          setStatus('Running…', 'running');
          break;
        case 'stderrChunk': {
          const stderrText = String(msg.text || '').trim();
          if (stderrText) {
            addBubble('stderr', stderrText);
          }
          break;
        }
        case 'assistantChunk':
          appendStream(msg.text || '');
          break;
        case 'toolStep': {
          try { addToolStep(msg); } catch (_) {}
          break;
        }
        case 'permissionRequest': {
          var permDiv = document.createElement('div');
          permDiv.className = 'msg permission-request';
          permDiv.innerHTML = '<div class="msg-role">🔐 Permission</div><div class="msg-body"><strong>Tool:</strong> ' + esc(msg.tool || 'unknown') + '<br>' + esc(msg.reason || '') + '</div>';
          thread.appendChild(permDiv);
          scrollBottom();
          break;
        }
        case 'permissionDenial': {
          var denyDiv = document.createElement('div');
          denyDiv.className = 'msg error';
          denyDiv.innerHTML = '<div class="msg-role">🚫 Denied</div><div class="msg-body"><strong>' + esc(msg.tool || 'tool') + ':</strong> ' + esc(msg.reason || 'Permission denied') + '</div>';
          thread.appendChild(denyDiv);
          scrollBottom();
          break;
        }
        case 'reasoningStep': {
          try { addReasoningStep(msg.step); } catch (_) {}
          break;
        }
        case 'decisioningEvent': {
          try { addDecisioningEvent(msg.event); } catch (_) {}
          break;
        }

        case 'assistantDone':
          endStream();
          state.streaming = false;
          updateSendButtonState();
          if (!state.lastRunFailed) {
            setStatus('Done', 'done');
          }
          break;
        case 'error':
          endStream();
          state.streaming = false;
          state.lastRunFailed = true;
          updateSendButtonState();
          addBubble('error', msg.text || 'Unknown error');
          setStatus('Error', 'error');
          break;
      }
    });

    /* ── init ── */
    trustBanner.hidden = state.isTrusted;
    setStatus(state.isTrusted ? 'Ready' : 'Workspace untrusted — execution blocked.', state.isTrusted ? '' : 'error');
    updateModelBar();
    updateSendButtonState();
    renderThread();
    try { updateReasoningToggle(); } catch (_) {}
    try { vscode.postMessage({ type: 'ready' }); } catch (_) {}

    // Self-check: if model label remained at the placeholder, try re-requesting bootstrap after a short delay.
    setTimeout(function() {
      try {
        if (modelLabel && (modelLabel.textContent === 'Loading…' || !modelLabel.textContent)) {
          try { vscode.postMessage({ type: 'ready' }); } catch (_) {}
        }
      } catch (_) {}
    }, 3000);
  })();
