// Run log viewer: streams the plain text log and only keeps lines near the screen in the page.
"use strict";
(function () {
  const LINES_PER_BLOCK = 500;

  // Splits streamed text into lines and hands them on in blocks of `size` lines.
  function lineSplitter(size, onBlock) {
    let pending = [];
    let rest = "";
    return {
      push(text) {
        const lines = (rest + text).split("\n");
        rest = lines.pop();
        pending = pending.concat(lines);
        while (pending.length >= size) {
          onBlock(pending.splice(0, size));
        }
      },
      end() {
        if (rest) {
          pending.push(rest);
        }
        if (pending.length) {
          onBlock(pending);
        }
        pending = [];
        rest = "";
      },
    };
  }

  // Number of the next line after `start` that holds the lower case `text`, going round to the top; null if none.
  function findNext(texts, text, start) {
    if (!texts.length) {
      return null;
    }
    const first = Math.floor(start / LINES_PER_BLOCK);
    for (let k = 0; k <= texts.length; k++) {
      const index = (first + k) % texts.length;
      const block = texts[index].toLowerCase();
      if (!block.includes(text)) {
        continue;
      }
      const lines = block.split("\n");
      for (let i = 0; i < lines.length; i++) {
        const n = index * LINES_PER_BLOCK + i + 1;
        if ((k > 0 || n > start) && lines[i].includes(text)) {
          return n;
        }
      }
    }
    return null;
  }

  // Address of line `n` on the server's page of `pageLines` lines, so it also works without JavaScript.
  function lineUrl(n, pageLines) {
    return `?start=${Math.floor((n - 1) / pageLines) * pageLines + 1}#L${n}`;
  }

  function lineFromHash(hash) {
    const match = /^#L([1-9]\d*)$/.exec(hash);
    return match ? Number(match[1]) : null;
  }

  if (typeof module === "object") {
    module.exports = { LINES_PER_BLOCK, lineSplitter, findNext, lineUrl, lineFromHash };
    return;
  }

  const view = document.getElementById("log-lines");
  const status = document.getElementById("log-status");
  const links = document.getElementById("log-links");
  const find = document.getElementById("log-find");
  const query = find.elements.find;
  const pageLines = Number(view.dataset.pageLines);
  const lineHeight = parseFloat(getComputedStyle(view).lineHeight);
  const texts = [];
  let lineCount = 0;
  let selectedLine = Number(view.dataset.selected) || null;
  let followEnd = location.hash === "#log-end";
  // Once loaded, go to the line the server found, or show the server's page from its first line.
  let wantedLine = null;
  let wantedSelect = true;
  // Keep the server's find or past-the-end text; its "Lines A to B" text is about its own page.
  let serverStatus = status.hasAttribute("data-page-range") ? "" : status.textContent;
  if (!followEnd && view.dataset.selected) {
    wantedLine = Number(view.dataset.selected);
  } else if (!followEnd && Number(view.dataset.start) > 1) {
    wantedLine = Number(view.dataset.start);
    wantedSelect = false;
  }
  let loaded = false;
  // Blocks wait here while the server's page stays, until the lines on it have loaded.
  const waitingBlocks = [];
  let replaced = false;
  const serverLines = view.querySelectorAll("span[id]");
  const serverEnd = serverLines.length ? Number(serverLines[serverLines.length - 1].id.slice(1)) : 0;

  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          show(entry.target);
        } else {
          hide(entry.target);
        }
      }
    },
    { rootMargin: "1000px 0px" },
  );

  function show(block) {
    if (block.childElementCount) {
      return;
    }
    const index = Number(block.dataset.index);
    const fragment = document.createDocumentFragment();
    texts[index].split("\n").forEach((text, i) => {
      const n = index * LINES_PER_BLOCK + i + 1;
      const line = document.createElement("span");
      line.id = "L" + n;
      line.classList.toggle("selected", n === selectedLine);
      const number = document.createElement("a");
      number.href = lineUrl(n, pageLines);
      number.tabIndex = -1;
      number.textContent = n + " ";
      line.append(number, text + "\n");
      fragment.append(line);
    });
    block.style.height = "";
    block.replaceChildren(fragment);
  }

  function hide(block) {
    // Keep the lines that hold selected text, so the selection stays.
    const selection = getSelection();
    const selected = !selection.isCollapsed && selection.containsNode(block, true);
    if (block.childElementCount && !selected) {
      block.style.height = block.offsetHeight + "px";
      block.replaceChildren();
    }
  }

  function replacePage() {
    if (replaced) {
      return;
    }
    replaced = true;
    // Keep the line at the top of the screen where it is.
    const anchor = [...serverLines].find((line) => line.getBoundingClientRect().bottom > 0);
    const top = anchor?.getBoundingClientRect().top;
    view.replaceChildren(...waitingBlocks);
    view.classList.add("viewer");
    document.querySelectorAll(".log-nav").forEach((nav) => {
      nav.hidden = true;
    });
    const n = anchor ? Number(anchor.id.slice(1)) : null;
    if (n !== null && n <= lineCount) {
      showAround(n);
      scrollBy(0, document.getElementById("L" + n).getBoundingClientRect().top - top);
    }
  }

  // Fill the block of line `n` and the one above, so that their real height does not move the line later.
  function showAround(n) {
    const index = Math.floor((n - 1) / LINES_PER_BLOCK);
    for (const block of [view.children[index - 1], view.children[index]]) {
      if (block) {
        show(block);
      }
    }
  }

  function addBlock(lines) {
    const block = document.createElement("span");
    block.dataset.index = texts.length;
    block.style.height = lines.length * lineHeight + "px";
    texts.push(lines.join("\n"));
    lineCount += lines.length;
    view.style.setProperty("--digits", Math.max(4, String(lineCount).length) + "ch");
    if (replaced) {
      view.append(block);
    } else {
      waitingBlocks.push(block);
    }
    observer.observe(block);
    if (!replaced && wantedLine === null && !followEnd && lineCount >= serverEnd) {
      replacePage();
    }
    if (wantedLine !== null && wantedLine <= lineCount) {
      goToLine(wantedLine, wantedSelect);
    }
  }

  function goToLine(n, select = true) {
    if (n > lineCount) {
      wantedLine = n;
      wantedSelect = select;
      return;
    }
    replacePage();
    wantedLine = null;
    showAround(n);
    const line = document.getElementById("L" + n);
    if (!select) {
      line.scrollIntoView({ block: "start" });
      return;
    }
    document.getElementById("L" + selectedLine)?.classList.remove("selected");
    selectedLine = n;
    line.classList.add("selected");
    line.scrollIntoView({ block: "center" });
  }

  function goToEnd() {
    replacePage();
    if (view.lastElementChild) {
      show(view.lastElementChild);
    }
    document.getElementById("log-end").scrollIntoView();
  }

  function goToHash() {
    const n = lineFromHash(location.hash);
    if (n !== null) {
      goToLine(n);
    }
  }

  function searchStart() {
    const selected = document.getElementById("L" + selectedLine);
    if (selected) {
      const rect = selected.getBoundingClientRect();
      if (rect.bottom > 0 && rect.top < innerHeight) {
        return selectedLine;
      }
    }
    for (const line of view.querySelectorAll("span > span")) {
      if (line.getBoundingClientRect().bottom > 0) {
        return Number(line.id.slice(1)) - 1;
      }
    }
    return 0;
  }

  async function load() {
    status.textContent = "Loading the log...";
    const response = await fetch(location.pathname, {
      headers: { Accept: "text/plain" },
    });
    if (!response.ok) {
      status.textContent = `Could not load the log (error ${response.status}).`;
      find.hidden = true;
      return;
    }
    const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
    const splitter = lineSplitter(LINES_PER_BLOCK, addBlock);
    let lastPause = performance.now();
    for (;;) {
      if (performance.now() - lastPause > 50) {
        // Let the browser show the lines added so far.
        await new Promise((resolve) => setTimeout(resolve));
        lastPause = performance.now();
      }
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      splitter.push(value);
    }
    splitter.end();
    replacePage();
    loaded = true;
    if (status.textContent === "Loading the log...") {
      status.textContent = lineCount ? serverStatus : "The log is empty.";
    }
    links.hidden = !lineCount;
    find.hidden = !lineCount;
    if (wantedLine !== null && lineCount) {
      const count = lineCount === 1 ? "1 line" : `${lineCount} lines`;
      status.textContent = `There is no line ${wantedLine}. The log has ${count}.`;
    }
    if (followEnd) {
      goToEnd();
    }
  }

  document.querySelectorAll('a[href$="#log-end"]').forEach((link) => {
    link.addEventListener("click", (event) => {
      event.preventDefault();
      history.pushState(null, "", link.href);
      followEnd = true;
      goToEnd();
    });
  });
  for (const type of ["wheel", "touchstart", "keydown", "pointerdown"]) {
    window.addEventListener(type, () => {
      followEnd = false;
      // Once the reader scrolls, taps or types, the server's page stays until it can be swapped in place.
      if (!replaced && wantedLine !== null) {
        if (wantedSelect) {
          selectedLine = wantedLine;
        }
        wantedLine = null;
      }
    });
  }
  find.addEventListener("submit", (event) => {
    event.preventDefault();
    const text = query.value.toLowerCase();
    if (!text) {
      return;
    }
    const start = searchStart();
    const n = findNext(texts, text, start);
    if (n === null) {
      const where = loaded ? "the log" : "the lines loaded so far";
      status.textContent = `"${query.value}" is not in ${where}.`;
    } else {
      status.textContent = n > start ? `Found on line ${n}.` : `Found on line ${n} (searched again from the top).`;
      history.replaceState(null, "", lineUrl(n, pageLines));
      goToLine(n);
    }
  });
  // Line links stay on this page.
  view.addEventListener("click", (event) => {
    const link = event.target.closest("a");
    const n = link ? lineFromHash(link.hash) : null;
    if (n !== null) {
      event.preventDefault();
      history.pushState(null, "", lineUrl(n, pageLines));
      serverStatus = "";
      if (loaded) {
        status.textContent = "";
      }
      if (n > lineCount) {
        // Mark the line on the server's page until the viewer can go to it.
        document.getElementById("L" + selectedLine)?.classList.remove("selected");
        selectedLine = n;
        link.parentElement.classList.add("selected");
      }
      goToLine(n);
    }
  });
  // Ctrl+F goes to the find box: the browser's own search only sees the lines in the page.
  document.addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === "f" && !find.hidden && document.activeElement !== query) {
      event.preventDefault();
      query.focus();
      query.select();
    }
  });
  for (const type of ["hashchange", "popstate"]) {
    window.addEventListener(type, () => {
      serverStatus = "";
      if (loaded) {
        status.textContent = "";
      }
      goToHash();
    });
  }
  goToHash();
  load().catch((error) => {
    console.error(error);
    find.hidden = true;
    status.textContent = "Could not load the log. Try the raw log.";
  });
})();
