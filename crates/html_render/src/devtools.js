// What a developer's panel asks the page about itself. Everything here is read
// out of the page as it stands -- what it is made of, what its scripts said,
// what it fetched, what one element is painted with -- so there is no debugging
// protocol, nothing listening on a port, and nothing left running when the
// panel is closed.
(function () {
  if (window.__zedTools) {
    return;
  }

  var MOST_SAID = 500;
  var MOST_REQUESTS = 300;
  var MOST_ROWS = 3000;
  var LONGEST_TEXT = 2000;
  var LONGEST_BODY = 4000;
  var MARK = "data-zed-selection";

  function json(value) {
    try {
      return JSON.stringify(value);
    } catch (whatever) {
      return "null";
    }
  }

  /// Runs `fn`, and answers with `fallback` if the page will not have it. Half
  /// of what a panel wants to read is optional in some engine or forbidden on
  /// some page, and a panel that throws shows nothing at all.
  function safe(fn, fallback) {
    try {
      return fn();
    } catch (whatever) {
      return fallback;
    }
  }

  function cut(text, most) {
    text = String(text === null || text === undefined ? "" : text);
    return text.length > most ? text.slice(0, most) + "…" : text;
  }

  function ours(node) {
    return !!(node && node.getAttribute && node.getAttribute(MARK));
  }

  /// The nearest element that belongs to the page rather than to our own
  /// selection and outlines.
  function theirs(node) {
    while (node && ours(node)) {
      node = node.parentNode;
    }
    return node && node.nodeType === 1 ? node : null;
  }

  // ---------------------------------------------------------------- the console

  var said = [];
  var counters = {};
  var timers = {};
  var groupDepth = 0;

  /// How many names a page may count or time under. The names come from the
  /// page, so a page that makes a fresh one every turn would otherwise hold on
  /// to all of them for as long as it is open.
  var MOST_LABELS = 200;

  function under(labels, label) {
    if (!Object.prototype.hasOwnProperty.call(labels, label)) {
      var many = Object.keys(labels);
      if (many.length >= MOST_LABELS) {
        for (var i = 0; i < many.length; i++) {
          delete labels[many[i]];
        }
        remember("warn", ["More than " + MOST_LABELS + " names counted or timed; starting again."], "");
      }
    }
    return label;
  }

  /// One value as one line, the way a console shows it.
  function describe(piece) {
    if (typeof piece === "string") {
      return piece;
    }
    if (piece === null) {
      return "null";
    }
    if (piece === undefined) {
      return "undefined";
    }
    if (typeof piece === "function") {
      return "function " + (piece.name || "(anonymous)") + "()";
    }
    if (piece instanceof Error) {
      return piece.name + ": " + piece.message;
    }
    if (piece.nodeType === 1) {
      return "<" + piece.nodeName.toLowerCase() + ">";
    }
    if (piece.nodeType === 3) {
      return "#text " + cut(piece.data, 60);
    }
    // A page that logs a list of ten thousand things, or an object with a
    // thousand fields, must not have it written out in full on every line: the
    // writing out is the expensive part, and nobody reads past the first line
    // of it anyway.
    if (typeof piece.length === "number" && piece.length > 50) {
      return "Array(" + piece.length + ")";
    }
    var keys = safe(function () {
      return Object.keys(piece);
    }, []);
    if (keys.length > 50) {
      var kind = piece.constructor && piece.constructor.name
        ? piece.constructor.name
        : "Object";
      return kind + " {" + keys.length + " fields}";
    }
    return safe(function () {
      return cut(JSON.stringify(piece), 2000);
    }, String(piece));
  }

  /// A value opened one level, the way a console does when the reader asks for
  /// an object rather than a line about it.
  function describeDeeply(value) {
    if (value === null || value === undefined || typeof value !== "object") {
      return describe(value);
    }
    if (value.nodeType === 1) {
      return "<" + value.nodeName.toLowerCase() + ">  " + selectorOf(value);
    }
    if (typeof value.length === "number" && !value.nodeType) {
      var items = [];
      for (var i = 0; i < value.length && i < 20; i++) {
        items.push(describe(value[i]));
      }
      if (value.length > 20) {
        items.push("… " + (value.length - 20) + " more");
      }
      return "(" + value.length + ") [" + items.join(", ") + "]";
    }
    var keys = safe(function () {
      return Object.keys(value);
    }, []);
    var lines = [];
    for (var k = 0; k < keys.length && k < 30; k++) {
      lines.push("  " + keys[k] + ": " + cut(describe(value[keys[k]]), 200));
    }
    if (keys.length > 30) {
      lines.push("  … " + (keys.length - 30) + " more");
    }
    if (!lines.length) {
      return describe(value);
    }
    return (value.constructor && value.constructor.name ? value.constructor.name + " " : "") +
      "{\n" + lines.join("\n") + "\n}";
  }

  /// Where a line was said from, best effort: the page's own script and ours are
  /// in the same document, so this is the first frame that is not this file's.
  function whereFrom() {
    var stack = safe(function () {
      return new Error().stack || "";
    }, "");
    var frames = String(stack).split("\n");
    for (var i = 0; i < frames.length; i++) {
      var frame = frames[i].trim();
      if (frame && !/^(remember|describe|whereFrom|Error)/.test(frame) && frame.indexOf("__zed") < 0) {
        return cut(frame, 200);
      }
    }
    return "";
  }

  function remember(level, args, from) {
    var parts = [];
    for (var i = 0; i < args.length; i++) {
      parts.push(describe(args[i]));
    }
    var line = parts.join(" ");
    if (groupDepth > 0) {
      line = new Array(groupDepth + 1).join("· ") + line;
    }
    var last = said[said.length - 1];
    if (last && last.level === level && last.text === line) {
      last.times++;
      return;
    }
    said.push({
      level: level,
      text: cut(line, LONGEST_TEXT),
      at: Date.now(),
      from: from === undefined ? whereFrom() : from,
      times: 1
    });
    if (said.length > MOST_SAID) {
      said.shift();
    }
  }

  var plainly = { log: 1, info: 1, warn: 1, error: 1, debug: 1 };
  var wasConsole = {};
  for (var level in plainly) {
    (function (level) {
      wasConsole[level] = console[level];
      console[level] = function () {
        remember(level, arguments);
        if (wasConsole[level]) {
          wasConsole[level].apply(console, arguments);
        }
      };
    })(level);
  }

  /// The rest of the console's own interface. A page that calls any of these and
  /// finds nothing there breaks, so they are all answered whether or not the
  /// engine had them -- and whatever the engine did have is kept and called
  /// afterwards, so nothing the page could see is taken away.
  var alsoWas = {};
  ["trace", "dir", "assert", "count", "countReset", "time", "timeEnd", "timeLog",
   "group", "groupCollapsed", "groupEnd", "table", "clear"].forEach(function (name) {
    alsoWas[name] = console[name];
  });

  /// Hands the call on to whatever the engine had under that name.
  function alsoTell(name, args) {
    if (typeof alsoWas[name] === "function") {
      safe(function () {
        alsoWas[name].apply(console, args);
      });
    }
  }

  console.trace = function () {
    var parts = [];
    for (var i = 0; i < arguments.length; i++) {
      parts.push(describe(arguments[i]));
    }
    var stack = safe(function () {
      return String(new Error().stack || "").split("\n").slice(1, 6).join("\n");
    }, "");
    remember("trace", [(parts.join(" ") || "trace") + (stack ? "\n" + stack : "")], "");
    alsoTell("trace", arguments);
  };
  console.dir = function (value) {
    remember("log", [describeDeeply(value)]);
    alsoTell("dir", arguments);
  };
  console.assert = function (holds) {
    if (holds) {
      return;
    }
    var parts = ["Assertion failed"];
    for (var i = 1; i < arguments.length; i++) {
      parts.push(describe(arguments[i]));
    }
    remember("error", [parts.join(": ")]);
    alsoTell("assert", arguments);
  };
  console.count = function (label) {
    label = under(counters, label === undefined ? "default" : String(label));
    counters[label] = (counters[label] || 0) + 1;
    remember("log", [label + ": " + counters[label]]);
    alsoTell("count", arguments);
  };
  console.countReset = function (label) {
    counters[label === undefined ? "default" : String(label)] = 0;
    alsoTell("countReset", arguments);
  };
  console.time = function (label) {
    timers[under(timers, label === undefined ? "default" : String(label))] = performance.now();
    alsoTell("time", arguments);
  };
  console.timeEnd = function (label) {
    label = label === undefined ? "default" : String(label);
    var began = timers[label];
    if (began === undefined) {
      remember("warn", ["Timer " + label + " does not exist"]);
      return;
    }
    delete timers[label];
    remember("log", [label + ": " + (performance.now() - began).toFixed(2) + " ms"]);
    alsoTell("timeEnd", arguments);
  };
  console.timeLog = function (label) {
    label = label === undefined ? "default" : String(label);
    var began = timers[label];
    if (began !== undefined) {
      remember("log", [label + ": " + (performance.now() - began).toFixed(2) + " ms"]);
    }
    alsoTell("timeLog", arguments);
  };
  console.group = function () {
    remember("group", arguments);
    groupDepth++;
    alsoTell("group", arguments);
  };
  console.groupCollapsed = console.group;
  console.groupEnd = function () {
    groupDepth = Math.max(0, groupDepth - 1);
    alsoTell("groupEnd", arguments);
  };
  console.table = function (data) {
    if (!data || typeof data !== "object") {
      remember("log", [describe(data)]);
      return;
    }
    var rowKeys = safe(function () {
      return Object.keys(data);
    }, []);
    var columns = [];
    for (var r = 0; r < rowKeys.length && r < 50; r++) {
      var row = data[rowKeys[r]];
      if (row && typeof row === "object") {
        var keys = safe(function () {
          return Object.keys(row);
        }, []);
        for (var k = 0; k < keys.length; k++) {
          if (columns.indexOf(keys[k]) < 0 && columns.length < 12) {
            columns.push(keys[k]);
          }
        }
      }
    }
    var lines = ["(index)" + (columns.length ? " | " + columns.join(" | ") : " | value")];
    for (var i = 0; i < rowKeys.length && i < 50; i++) {
      var value = data[rowKeys[i]];
      var cells = [rowKeys[i]];
      if (columns.length) {
        for (var c = 0; c < columns.length; c++) {
          cells.push(cut(describe(value ? value[columns[c]] : ""), 40));
        }
      } else {
        cells.push(cut(describe(value), 60));
      }
      lines.push(cells.join(" | "));
    }
    remember("log", [lines.join("\n")], "");
    alsoTell("table", arguments);
  };
  console.clear = function () {
    said = [];
    remember("log", ["Console was cleared"], "");
    alsoTell("clear", arguments);
  };

  window.addEventListener("error", function (event) {
    remember(
      "error",
      [event.message + " (" + (event.filename || "") + ":" + (event.lineno || 0) + ")"],
      ""
    );
  });
  window.addEventListener("unhandledrejection", function (event) {
    remember("error", ["Unhandled promise rejection: " + describe(event.reason)], "");
  });

  // ----------------------------------------------------- what listens to events

  // Which elements listen for what. An engine keeps this to itself, so it is
  // counted as the page asks for it -- which is why the tools are put in the
  // page ahead of its own scripts.
  var listeners = typeof WeakMap === "function" ? new WeakMap() : null;
  var listenerCount = 0;
  var addListener = EventTarget.prototype.addEventListener;
  var removeListener = EventTarget.prototype.removeEventListener;

  /// Whether a registration asked for the capture phase, which is part of what
  /// makes one listener a different listener from another.
  function capturing(options) {
    return options === true || !!(options && options.capture);
  }

  function whichListener(list, handler, capture) {
    for (var i = 0; i < list.length; i++) {
      if (list[i].handler === handler && list[i].capture === capture) {
        return i;
      }
    }
    return -1;
  }

  EventTarget.prototype.addEventListener = function (type, handler, options) {
    if (listeners && this && this.nodeType === 1) {
      var mine = listeners.get(this) || {};
      var already = mine[type] || [];
      // The page's own list ignores a repeat of the same handler in the same
      // phase, so counting it twice would say a page listens for more than it
      // does.
      if (whichListener(already, handler, capturing(options)) < 0) {
        already.push({ handler: handler, capture: capturing(options) });
        mine[type] = already;
        listeners.set(this, mine);
        listenerCount++;
      }
    }
    return addListener.apply(this, arguments);
  };
  EventTarget.prototype.removeEventListener = function (type, handler, options) {
    if (listeners && this && this.nodeType === 1) {
      var mine = listeners.get(this);
      var already = mine && mine[type];
      if (already) {
        var which = whichListener(already, handler, capturing(options));
        if (which >= 0) {
          already.splice(which, 1);
          listenerCount = Math.max(0, listenerCount - 1);
          if (!already.length) {
            delete mine[type];
          }
        }
      }
    }
    return removeListener.apply(this, arguments);
  };

  // ------------------------------------------------------------- what it fetched

  var requests = [];
  var nextRequest = 1;

  function record(method, url, kind) {
    var entry = {
      id: nextRequest++,
      method: String(method || "GET").toUpperCase(),
      url: String(url),
      kind: kind,
      status: 0,
      statusText: "",
      size: 0,
      ms: 0,
      start: Math.round(performance.now()),
      type: "",
      reqHeaders: [],
      resHeaders: [],
      body: ""
    };
    requests.push(entry);
    if (requests.length > MOST_REQUESTS) {
      requests.shift();
    }
    return entry;
  }

  function headerPairs(headers) {
    var out = [];
    if (!headers) {
      return out;
    }
    safe(function () {
      if (typeof headers.forEach === "function") {
        headers.forEach(function (value, name) {
          out.push([String(name), cut(value, 300)]);
        });
        return;
      }
      var keys = Object.keys(headers);
      for (var i = 0; i < keys.length; i++) {
        out.push([keys[i], cut(headers[keys[i]], 300)]);
      }
    });
    return out;
  }

  function splitHeaders(text) {
    var out = [];
    var lines = String(text || "").split(/\r?\n/);
    for (var i = 0; i < lines.length; i++) {
      var at = lines[i].indexOf(":");
      if (at > 0) {
        out.push([lines[i].slice(0, at).trim(), cut(lines[i].slice(at + 1).trim(), 300)]);
      }
    }
    return out;
  }

  function headerOf(pairs, name) {
    for (var i = 0; i < pairs.length; i++) {
      if (String(pairs[i][0]).toLowerCase() === name) {
        return pairs[i][1];
      }
    }
    return "";
  }

  if (typeof window.fetch === "function") {
    var wasFetch = window.fetch;
    window.fetch = function (input, init) {
      var url = input && input.url ? input.url : String(input);
      var method = (init && init.method) || (input && input.method) || "GET";
      var entry = record(method, url, "fetch");
      entry.reqHeaders = headerPairs(init && init.headers ? init.headers : input && input.headers);
      var began = performance.now();
      return wasFetch.apply(this, arguments).then(
        function (response) {
          entry.ms = Math.round(performance.now() - began);
          entry.status = response.status;
          entry.statusText = response.statusText || "";
          entry.resHeaders = headerPairs(response.headers);
          entry.type = headerOf(entry.resHeaders, "content-type");
          // Read through a copy: the body belongs to whoever asked for it, and
          // reading the original would leave the page with an empty response.
          safe(function () {
            response.clone().text().then(function (text) {
              entry.size = text.length;
              entry.body = cut(text, LONGEST_BODY);
            }, function () {});
          });
          return response;
        },
        function (error) {
          entry.ms = Math.round(performance.now() - began);
          entry.statusText = String((error && error.message) || error);
          throw error;
        }
      );
    };
  }

  if (typeof window.XMLHttpRequest === "function") {
    var wasOpen = XMLHttpRequest.prototype.open;
    var wasSend = XMLHttpRequest.prototype.send;
    var wasSetHeader = XMLHttpRequest.prototype.setRequestHeader;
    XMLHttpRequest.prototype.open = function (method, url) {
      this.__zedRequest = record(method, url, "xhr");
      return wasOpen.apply(this, arguments);
    };
    XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
      if (this.__zedRequest) {
        this.__zedRequest.reqHeaders.push([String(name), cut(value, 300)]);
      }
      return wasSetHeader.apply(this, arguments);
    };
    XMLHttpRequest.prototype.send = function () {
      var entry = this.__zedRequest;
      if (entry) {
        var request = this;
        var began = performance.now();
        addListener.call(this, "loadend", function () {
          entry.ms = Math.round(performance.now() - began);
          entry.status = request.status;
          entry.statusText = request.statusText || "";
          entry.resHeaders = splitHeaders(safe(function () {
            return request.getAllResponseHeaders();
          }, ""));
          entry.type = headerOf(entry.resHeaders, "content-type");
          var text = safe(function () {
            return request.responseText || "";
          }, "");
          entry.size = text.length;
          entry.body = cut(text, LONGEST_BODY);
        });
      }
      return wasSend.apply(this, arguments);
    };
  }

  function timingFor(url) {
    var entries = safe(function () {
      return performance.getEntriesByName(url);
    }, []) || [];
    for (var i = entries.length - 1; i >= 0; i--) {
      if (entries[i].entryType === "resource") {
        return entries[i];
      }
    }
    return null;
  }

  function phasesOf(entry) {
    if (!entry) {
      return [];
    }
    var span = function (from, to) {
      var was = entry[from];
      var now = entry[to];
      return was && now && now > was ? Math.round((now - was) * 100) / 100 : 0;
    };
    return [
      ["redirect", span("redirectStart", "redirectEnd")],
      ["dns", span("domainLookupStart", "domainLookupEnd")],
      ["connect", span("connectStart", "connectEnd")],
      ["tls", span("secureConnectionStart", "connectEnd")],
      ["wait", span("requestStart", "responseStart")],
      ["receive", span("responseStart", "responseEnd")]
    ];
  }

  /// Every request the page has made, as one row each. What went through the
  /// page's own interfaces is known in full; the rest -- images, stylesheets,
  /// fonts, which the engine fetches itself -- is what the page's timing says.
  function network() {
    var rows = [];
    var seen = {};
    for (var i = 0; i < requests.length; i++) {
      var request = requests[i];
      seen[request.url] = true;
      rows.push({
        id: request.id,
        method: request.method,
        url: request.url,
        kind: request.kind,
        status: request.status,
        size: request.size,
        ms: request.ms,
        start: request.start,
        type: request.type
      });
    }
    var entries = safe(function () {
      return performance.getEntriesByType("resource");
    }, []) || [];
    var first = Math.max(0, entries.length - MOST_REQUESTS);
    for (var e = first; e < entries.length; e++) {
      var entry = entries[e];
      if (seen[entry.name]) {
        continue;
      }
      rows.push({
        id: -(e + 1),
        method: "GET",
        url: entry.name,
        kind: entry.initiatorType || "other",
        status: 0,
        size: entry.transferSize || entry.encodedBodySize || 0,
        ms: Math.round(entry.duration),
        start: Math.round(entry.startTime),
        type: ""
      });
    }
    rows.sort(function (left, right) {
      return left.start - right.start;
    });
    return json(rows);
  }

  /// One request in full: its headers both ways, what it cost in each stage, and
  /// as much of the answer as is worth showing.
  function request(id) {
    id = Number(id);
    if (id < 0) {
      var entries = safe(function () {
        return performance.getEntriesByType("resource");
      }, []) || [];
      var entry = entries[-id - 1];
      if (!entry) {
        return "{}";
      }
      return json({
        url: entry.name,
        method: "GET",
        status: 0,
        statusText: "fetched by the engine, not by the page",
        type: entry.initiatorType || "",
        size: entry.transferSize || entry.encodedBodySize || 0,
        ms: Math.round(entry.duration),
        reqHeaders: [],
        resHeaders: [],
        phases: phasesOf(entry),
        body: ""
      });
    }
    for (var i = 0; i < requests.length; i++) {
      if (requests[i].id === id) {
        var found = requests[i];
        return json({
          url: found.url,
          method: found.method,
          status: found.status,
          statusText: found.statusText,
          type: found.type,
          size: found.size,
          ms: found.ms,
          reqHeaders: found.reqHeaders,
          resHeaders: found.resHeaders,
          phases: phasesOf(timingFor(found.url)),
          body: found.body
        });
      }
    }
    return "{}";
  }

  // ------------------------------------------------------- what the page is made of

  /// Whether a change to the page was one of ours -- an outline drawn around an
  /// element, a grid over it, the numbers over its tab order -- rather than the
  /// page's own doing. Ours must not send the panel the whole page again.
  function ourDoing(record) {
    if (ours(record.target)) {
      return true;
    }
    var lists = [record.addedNodes, record.removedNodes];
    var any = false;
    for (var list = 0; list < lists.length; list++) {
      var nodes = lists[list];
      for (var node = 0; nodes && node < nodes.length; node++) {
        any = true;
        if (!ours(nodes[node])) {
          return false;
        }
      }
    }
    return any;
  }

  var numbered = [];
  // Whether the page has changed since the tree was last handed over. Counted
  // rather than flagged, so a change during the walk is not lost.
  var changed = 1;
  var readAt = 0;

  if (typeof MutationObserver === "function") {
    var watching = new MutationObserver(function (records) {
      for (var i = 0; i < records.length; i++) {
        if (!ourDoing(records[i])) {
          changed++;
          return;
        }
      }
    });
    safe(function () {
      watching.observe(document.documentElement, {
        childList: true,
        subtree: true,
        attributes: true
      });
    });
  } else {
    // Without one, every ask is answered with the whole tree, which is what a
    // panel would have done anyway.
    changed = Number.MAX_SAFE_INTEGER;
  }

  /// What an element is called in the tree and in the crumbs: its tag, the id
  /// it goes by, and the classes it carries.
  function nameOf(node) {
    var name = node.nodeName.toLowerCase();
    var mark = node.id ? "#" + node.id : "";
    var classes = node.className && typeof node.className === "string"
      ? "." + node.className.trim().split(/\s+/).slice(0, 3).join(".")
      : "";
    return name + mark + (classes === "." ? "" : classes);
  }

  function walkTree(deepest) {
    var rows = [];
    numbered = [];
    var walk = function (node, depth) {
      if (rows.length >= MOST_ROWS || depth > deepest) {
        return;
      }
      if (node.nodeType !== 1 || ours(node)) {
        return;
      }
      var children = 0;
      for (var c = 0; c < node.children.length; c++) {
        if (!ours(node.children[c])) {
          children++;
        }
      }
      numbered.push(node);
      var listening = listeners ? listeners.get(node) : null;
      rows.push({
        at: numbered.length - 1,
        depth: depth,
        text: nameOf(node),
        children: children,
        preview: children ? "" : cut(String(node.textContent || "").trim().replace(/\s+/g, " "), 60),
        listens: listening ? Object.keys(listening).length : 0
      });
      for (var i = 0; i < node.children.length; i++) {
        walk(node.children[i], depth + 1);
      }
    };
    walk(document.documentElement, 0);
    window.__zedNumbered = numbered;
    return rows;
  }

  function tree(deepest) {
    readAt = changed;
    return json(walkTree(deepest || 12));
  }

  /// The tree, but only if the page is not the one the panel already has.
  ///
  /// A panel asks again every few hundred milliseconds, and a page can be three
  /// thousand elements: handing all of them over each time is work the page pays
  /// for and nothing the reader sees. Nothing back means nothing has changed,
  /// which also leaves the numbering the panel is holding valid.
  function treeIfChanged(deepest) {
    if (changed === readAt) {
      return "";
    }
    return tree(deepest);
  }

  function nodeAt(at) {
    return numbered[Number(at)] || null;
  }

  /// A selector that finds this one element again, the way a panel's "copy
  /// selector" gives.
  function selectorOf(node) {
    if (!node || node.nodeType !== 1) {
      return "";
    }
    var parts = [];
    while (node && node.nodeType === 1 && parts.length < 8) {
      if (node.id) {
        parts.unshift("#" + node.id);
        break;
      }
      var part = node.nodeName.toLowerCase();
      var parent = node.parentNode;
      if (parent && parent.children) {
        var same = 0;
        var index = 0;
        for (var i = 0; i < parent.children.length; i++) {
          if (parent.children[i].nodeName === node.nodeName) {
            same++;
            if (parent.children[i] === node) {
              index = same;
            }
          }
        }
        if (same > 1) {
          part += ":nth-of-type(" + index + ")";
        }
      }
      parts.unshift(part);
      node = parent && parent.nodeType === 1 ? parent : null;
    }
    return parts.join(" > ");
  }

  var WANTED = [
    "display", "position", "width", "height", "margin", "padding", "border",
    "color", "background-color", "font", "flex", "grid-template-columns",
    "overflow", "z-index", "opacity"
  ];

  /// Everything about one element that a panel shows first: where it is, how big
  /// it is, and the handful of properties that explain most of it.
  function about(at) {
    var node = nodeAt(at);
    if (!node) {
      return "{}";
    }
    var box = node.getBoundingClientRect();
    var style = safe(function () {
      return window.getComputedStyle(node);
    }, null);
    var styles = {};
    for (var i = 0; i < WANTED.length; i++) {
      styles[WANTED[i]] = style ? style.getPropertyValue(WANTED[i]) : "";
    }
    return json({
      tag: node.nodeName.toLowerCase(),
      selector: selectorOf(node),
      box: {
        left: Math.round(box.left),
        top: Math.round(box.top),
        width: Math.round(box.width),
        height: Math.round(box.height)
      },
      html: cut(node.outerHTML || "", 400),
      styles: styles
    });
  }

  /// The path from the page's root down to one element, for a row of crumbs.
  function path(at) {
    var node = nodeAt(at);
    var crumbs = [];
    while (node && node.nodeType === 1) {
      crumbs.unshift({ at: numbered.indexOf(node), text: nameOf(node) });
      node = node.parentNode;
    }
    return json(crumbs);
  }

  function declarationsOf(rule) {
    var out = [];
    var style = rule.style;
    if (style && style.length) {
      for (var i = 0; i < style.length; i++) {
        var name = style.item(i);
        out.push([name, cut(style.getPropertyValue(name), 200)]);
      }
      return out;
    }
    // Some rules will not enumerate; their text still says what they set.
    var text = String(rule.cssText || "");
    var inside = text.slice(text.indexOf("{") + 1, text.lastIndexOf("}"));
    var pieces = inside.split(";");
    for (var p = 0; p < pieces.length; p++) {
      var at = pieces[p].indexOf(":");
      if (at > 0) {
        out.push([pieces[p].slice(0, at).trim(), cut(pieces[p].slice(at + 1).trim(), 200)]);
      }
    }
    return out;
  }

  var PSEUDO = /::?(before|after|first-line|first-letter|placeholder|selection|marker|backdrop|part\([^)]*\)|slotted\([^)]*\))/g;

  function matchesAny(node, selector) {
    var parts = String(selector).split(",");
    for (var i = 0; i < parts.length; i++) {
      var one = parts[i].replace(PSEUDO, "").trim();
      if (!one) {
        continue;
      }
      var hit = safe(function () {
        return node.matches(one);
      }, false);
      if (hit) {
        return true;
      }
    }
    return false;
  }

  function sheetName(sheet, index) {
    if (sheet.href) {
      var tail = String(sheet.href).split("/");
      return tail[tail.length - 1] || String(sheet.href);
    }
    return "inline stylesheet " + (index + 1);
  }

  function collectRules(cssRules, where, into, node, media) {
    for (var i = 0; i < cssRules.length; i++) {
      var rule = cssRules[i];
      if (rule.selectorText) {
        if (matchesAny(node, rule.selectorText)) {
          into.push({
            sheet: where,
            selector: String(rule.selectorText),
            media: media,
            declarations: declarationsOf(rule)
          });
        }
      } else if (rule.cssRules) {
        var condition = rule.conditionText ||
          (rule.media && rule.media.mediaText) ||
          String(rule.cssText || "").split("{")[0].trim();
        collectRules(rule.cssRules, where, into, node, cut(condition, 120));
      }
    }
  }

  /// Which rules of the page's own stylesheets reach this element, and what each
  /// of them sets. Later ones win, so they are answered in the order the cascade
  /// reads them and the panel shows the last first.
  function rules(at) {
    var node = nodeAt(at);
    if (!node) {
      return "[]";
    }
    var found = [];
    var sheets = safe(function () {
      return document.styleSheets;
    }, null);
    for (var s = 0; sheets && s < sheets.length; s++) {
      var sheet = sheets[s];
      var cssRules = safe(function () {
        return sheet.cssRules;
      }, null);
      if (!cssRules) {
        // A stylesheet from somewhere else will not open to a script, which is
        // the browser's rule and not something to work around.
        found.push({ sheet: sheetName(sheet, s), selector: "(not readable from here)", media: "", declarations: [] });
        continue;
      }
      collectRules(cssRules, sheetName(sheet, s), found, node, "");
    }
    var inline = safe(function () {
      return node.getAttribute("style");
    }, "");
    if (inline) {
      found.push({
        sheet: "element",
        selector: "style attribute",
        media: "",
        declarations: declarationsOf({ style: node.style, cssText: "{" + inline + "}" })
      });
    }
    return json(found);
  }

  /// Every property the element is actually painted with, which is what a
  /// computed view is: no cascade, no source, just the answer.
  function computed(at) {
    var node = nodeAt(at);
    if (!node) {
      return "[]";
    }
    var style = safe(function () {
      return window.getComputedStyle(node);
    }, null);
    if (!style) {
      return "[]";
    }
    var out = [];
    var seen = {};
    for (var i = 0; i < style.length; i++) {
      var name = style.item(i);
      if (name && !seen[name]) {
        seen[name] = true;
        out.push([name, cut(style.getPropertyValue(name), 200)]);
      }
    }
    for (var w = 0; w < WANTED.length; w++) {
      if (!seen[WANTED[w]]) {
        out.push([WANTED[w], cut(style.getPropertyValue(WANTED[w]), 200)]);
      }
    }
    out.sort(function (left, right) {
      return left[0] < right[0] ? -1 : left[0] > right[0] ? 1 : 0;
    });
    return json(out);
  }

  function sides(style, prefix, suffix) {
    var read = function (side) {
      var value = parseFloat(style.getPropertyValue(prefix + "-" + side + (suffix || "")));
      return isNaN(value) ? 0 : Math.round(value * 100) / 100;
    };
    return [read("top"), read("right"), read("bottom"), read("left")];
  }

  /// The box the element occupies, taken apart the way a layout view draws it.
  function layout(at) {
    var node = nodeAt(at);
    if (!node) {
      return "{}";
    }
    var box = node.getBoundingClientRect();
    var style = safe(function () {
      return window.getComputedStyle(node);
    }, null);
    if (!style) {
      return "{}";
    }
    var display = style.getPropertyValue("display");
    var answer = {
      box: {
        left: Math.round(box.left),
        top: Math.round(box.top),
        width: Math.round(box.width * 100) / 100,
        height: Math.round(box.height * 100) / 100
      },
      margin: sides(style, "margin"),
      border: sides(style, "border", "-width"),
      padding: sides(style, "padding"),
      content: {
        width: Math.round(parseFloat(style.getPropertyValue("width")) * 100) / 100 || 0,
        height: Math.round(parseFloat(style.getPropertyValue("height")) * 100) / 100 || 0
      },
      display: display,
      position: style.getPropertyValue("position"),
      boxSizing: style.getPropertyValue("box-sizing"),
      zIndex: style.getPropertyValue("z-index"),
      overflow: style.getPropertyValue("overflow"),
      flex: null,
      grid: null
    };
    if (display.indexOf("flex") >= 0) {
      answer.flex = {
        direction: style.getPropertyValue("flex-direction"),
        wrap: style.getPropertyValue("flex-wrap"),
        justify: style.getPropertyValue("justify-content"),
        align: style.getPropertyValue("align-items"),
        gap: style.getPropertyValue("gap")
      };
    }
    if (display.indexOf("grid") >= 0) {
      answer.grid = {
        columns: style.getPropertyValue("grid-template-columns"),
        rows: style.getPropertyValue("grid-template-rows"),
        gap: style.getPropertyValue("gap"),
        areas: cut(style.getPropertyValue("grid-template-areas"), 200)
      };
    }
    return json(answer);
  }

  /// What this element listens for, counted as its scripts asked, plus the
  /// handlers written into the markup itself.
  function listening(at) {
    var node = nodeAt(at);
    if (!node) {
      return "[]";
    }
    var out = [];
    var mine = listeners ? listeners.get(node) : null;
    if (mine) {
      for (var type in mine) {
        out.push([type, mine[type].length, "script"]);
      }
    }
    for (var a = 0; node.attributes && a < node.attributes.length; a++) {
      var name = node.attributes[a].name;
      if (name.indexOf("on") === 0 && name.length > 2) {
        out.push([name.slice(2), 1, "markup"]);
      }
    }
    return json(out);
  }

  /// The fonts one element is painted with, and the faces the page has loaded.
  /// A font that is asked for and never arrives is why text turns up in the
  /// wrong one, so what each face's state is matters as much as its name.
  function fonts(at) {
    var node = nodeAt(at);
    var style = node
      ? safe(function () {
          return window.getComputedStyle(node);
        }, null)
      : null;
    var faces = [];
    safe(function () {
      if (document.fonts && document.fonts.forEach) {
        document.fonts.forEach(function (face) {
          if (faces.length >= 100) {
            return;
          }
          faces.push({
            family: String(face.family || "").replace(/^["']|["']$/g, ""),
            weight: String(face.weight || ""),
            style: String(face.style || ""),
            status: String(face.status || "")
          });
        });
      }
    });
    return json({
      element: style
        ? {
            family: style.getPropertyValue("font-family"),
            size: style.getPropertyValue("font-size"),
            weight: style.getPropertyValue("font-weight"),
            style: style.getPropertyValue("font-style"),
            height: style.getPropertyValue("line-height"),
            spacing: style.getPropertyValue("letter-spacing")
          }
        : null,
      faces: faces
    });
  }

  var serviceWorkers = [];

  function askAboutWorkers() {
    safe(function () {
      if (navigator.serviceWorker && navigator.serviceWorker.getRegistrations) {
        navigator.serviceWorker.getRegistrations().then(function (found) {
          serviceWorkers = (found || []).map(function (one) {
            var worker = one.active || one.waiting || one.installing;
            return String(one.scope || "") + (worker ? "  " + String(worker.state) : "  (none)");
          });
        }, function () {});
      }
    });
  }

  /// What the page has installed to go on working without the network: the
  /// workers it registered, and the manifest that makes it installable.
  function installed() {
    askAboutWorkers();
    return json({
      manifest: safe(function () {
        var link = document.querySelector('link[rel="manifest"]');
        return link ? String(link.href) : "";
      }, ""),
      workers: serviceWorkers,
      supported: !!(navigator.serviceWorker && navigator.serviceWorker.getRegistrations)
    });
  }

  function html(at) {
    var node = nodeAt(at);
    return node ? cut(node.outerHTML || "", 8000) : "";
  }

  function selector(at) {
    return selectorOf(nodeAt(at));
  }

  /// Puts the markup the reader typed in place of one element. Answers with
  /// what went wrong, or with nothing at all when it worked -- the page is
  /// where the result is to be seen, not here.
  function setHtml(at, markup) {
    var node = nodeAt(at);
    if (!node) {
      return "There is no such element any more.";
    }
    if (node === document.documentElement || node === document.body) {
      return "The page's root and its body cannot be replaced this way.";
    }
    if (!node.parentNode) {
      return "That element is no longer in the page.";
    }
    try {
      node.outerHTML = String(markup);
    } catch (whatever) {
      return String((whatever && whatever.message) || whatever);
    }
    return "";
  }

  function bring(at) {
    var node = nodeAt(at);
    if (node && node.scrollIntoView) {
      node.scrollIntoView({ block: "center" });
    }
    return "";
  }

  function remove(at) {
    var node = nodeAt(at);
    if (node && node.parentNode && node !== document.documentElement) {
      node.parentNode.removeChild(node);
      return "gone";
    }
    return "";
  }

  // ------------------------------------------------------------------ the outline

  var outline = null;
  var badge = null;

  function overlay(kind) {
    var made = document.createElement("div");
    made.setAttribute(MARK, kind);
    made.style.cssText = "position:fixed;pointer-events:none;z-index:2147483646";
    (document.body || document.documentElement).appendChild(made);
    return made;
  }

  /// Draws a frame around an element, the way a browser's inspector does, with
  /// its size beside it.
  function highlight(at) {
    var node = nodeAt(at);
    if (!outline) {
      outline = overlay("outline");
      outline.style.cssText +=
        ";border:2px solid rgba(64,138,240,0.9);background:rgba(64,138,240,0.12)";
      badge = overlay("outline-size");
      badge.style.cssText +=
        ";background:rgba(64,138,240,0.95);color:#fff;font:11px/14px monospace;padding:1px 4px";
    }
    if (!node) {
      outline.style.display = "none";
      badge.style.display = "none";
      return "";
    }
    var box = node.getBoundingClientRect();
    outline.style.display = "block";
    outline.style.left = box.left + "px";
    outline.style.top = box.top + "px";
    outline.style.width = box.width + "px";
    outline.style.height = box.height + "px";
    badge.style.display = "block";
    badge.style.left = box.left + "px";
    badge.style.top = Math.max(0, box.top - 16) + "px";
    badge.textContent = node.nodeName.toLowerCase() + " " +
      Math.round(box.width) + "×" + Math.round(box.height);
    return "";
  }

  // ------------------------------------------------------------------- the picker

  var picking = false;
  var pickedNode = null;

  function pointedAt(event) {
    var node = event.target;
    if (!node || node.nodeType !== 1) {
      node = document.elementFromPoint(event.clientX, event.clientY);
    }
    return theirs(node);
  }

  addListener.call(window, "mousemove", function (event) {
    if (!picking) {
      return;
    }
    var node = pointedAt(event);
    if (!node) {
      return;
    }
    // Numbered as it is pointed at, so that what the panel is told to show
    // is a row it already has.
    var at = numbered.indexOf(node);
    if (at < 0) {
      walkTree(40);
      at = numbered.indexOf(node);
    }
    if (at >= 0) {
      highlight(at);
    }
  }, true);

  addListener.call(window, "click", function (event) {
    if (!picking) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    if (event.stopImmediatePropagation) {
      event.stopImmediatePropagation();
    }
    pickedNode = pointedAt(event);
    picking = false;
  }, true);

  /// Arms the picker: the next click in the page picks whatever is under it
  /// instead of reaching the page.
  function pick(on) {
    picking = String(on) !== "0" && String(on) !== "false";
    if (!picking) {
      highlight(-1);
    }
    return picking ? "armed" : "off";
  }

  /// What the reader picked, once. The page is walked again if need be, so the
  /// answer is a row the panel can show even if the element was deeper than the
  /// tree it had.
  function picked() {
    if (!pickedNode) {
      return "";
    }
    var node = pickedNode;
    pickedNode = null;
    var at = numbered.indexOf(node);
    if (at < 0) {
      walkTree(40);
      at = numbered.indexOf(node);
    }
    if (at < 0) {
      return "";
    }
    window.$0 = node;
    return json({ at: at, selector: selectorOf(node) });
  }

  // -------------------------------------------------------------- the stylesheets

  function sheets() {
    var out = [];
    var all = safe(function () {
      return document.styleSheets;
    }, null);
    for (var i = 0; all && i < all.length; i++) {
      var sheet = all[i];
      var cssRules = safe(function () {
        return sheet.cssRules;
      }, null);
      out.push({
        id: i,
        name: sheetName(sheet, i),
        href: String(sheet.href || ""),
        rules: cssRules ? cssRules.length : 0,
        disabled: !!sheet.disabled,
        media: safe(function () {
          return String(sheet.media && sheet.media.mediaText ? sheet.media.mediaText : "");
        }, ""),
        readable: !!cssRules
      });
    }
    return json(out);
  }

  function sheet(id) {
    var all = safe(function () {
      return document.styleSheets;
    }, null);
    var one = all && all[Number(id)];
    if (!one) {
      return "{}";
    }
    var cssRules = safe(function () {
      return one.cssRules;
    }, null);
    var lines = [];
    for (var i = 0; cssRules && i < cssRules.length && i < 500; i++) {
      lines.push(String(cssRules[i].cssText || ""));
    }
    return json({
      name: sheetName(one, Number(id)),
      href: String(one.href || ""),
      disabled: !!one.disabled,
      text: cut(lines.join("\n"), 40000),
      rules: cssRules ? cssRules.length : 0
    });
  }

  /// Turns one stylesheet off, or back on. This is the whole of what a style
  /// editor's eye does, and the page repaints without it.
  function toggleSheet(id) {
    var all = safe(function () {
      return document.styleSheets;
    }, null);
    var one = all && all[Number(id)];
    if (!one) {
      return "";
    }
    one.disabled = !one.disabled;
    return one.disabled ? "off" : "on";
  }

  // ------------------------------------------------------------------ the storage

  var databases = [];
  var cacheNames = [];

  function askAboutStores() {
    safe(function () {
      if (indexedDB && indexedDB.databases) {
        indexedDB.databases().then(function (found) {
          databases = (found || []).map(function (one) {
            return String(one.name) + (one.version ? " (v" + one.version + ")" : "");
          });
        }, function () {});
      }
    });
    safe(function () {
      if (typeof caches !== "undefined" && caches.keys) {
        caches.keys().then(function (found) {
          cacheNames = found || [];
        }, function () {});
      }
    });
  }

  function pairsOf(store) {
    var out = [];
    if (!store) {
      return out;
    }
    safe(function () {
      for (var i = 0; i < store.length && i < 500; i++) {
        var key = store.key(i);
        out.push([String(key), cut(store.getItem(key), 500)]);
      }
    });
    return out;
  }

  function storage() {
    askAboutStores();
    var cookies = [];
    safe(function () {
      var text = String(document.cookie || "");
      if (!text) {
        return;
      }
      var parts = text.split(";");
      for (var i = 0; i < parts.length; i++) {
        var at = parts[i].indexOf("=");
        var name = at > 0 ? parts[i].slice(0, at) : parts[i];
        cookies.push([name.trim(), at > 0 ? cut(parts[i].slice(at + 1), 500) : ""]);
      }
    });
    return json({
      cookies: cookies,
      local: pairsOf(safe(function () {
        return window.localStorage;
      }, null)),
      session: pairsOf(safe(function () {
        return window.sessionStorage;
      }, null)),
      databases: databases,
      caches: cacheNames
    });
  }

  /// Takes one entry out of a store, the way a storage panel's delete does.
  function forget(kind, key) {
    kind = String(kind);
    key = String(key);
    if (kind === "local") {
      safe(function () {
        window.localStorage.removeItem(key);
      });
    } else if (kind === "session") {
      safe(function () {
        window.sessionStorage.removeItem(key);
      });
    } else if (kind === "cookie") {
      safe(function () {
        document.cookie = key + "=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/";
      });
    }
    return "";
  }

  function clearStore(kind) {
    kind = String(kind);
    if (kind === "local") {
      safe(function () {
        window.localStorage.clear();
      });
    } else if (kind === "session") {
      safe(function () {
        window.sessionStorage.clear();
      });
    } else if (kind === "cookie") {
      safe(function () {
        var parts = String(document.cookie || "").split(";");
        for (var i = 0; i < parts.length; i++) {
          var at = parts[i].indexOf("=");
          var name = (at > 0 ? parts[i].slice(0, at) : parts[i]).trim();
          document.cookie = name + "=; expires=Thu, 01 Jan 1970 00:00:00 GMT; path=/";
        }
      });
    }
    return "";
  }

  // -------------------------------------------------------------- what it cost

  var heldAt = -1;
  var held = null;

  /// How much of everything the page holds. Counting it walks the whole page and
  /// reads all of its text, so it is counted again only once the page has
  /// changed -- otherwise a panel left open on the performance tab would walk a
  /// three thousand element page twice a second for numbers that had not moved.
  function whatItHolds() {
    if (held && heldAt === changed) {
      return held;
    }
    var sheetList = safe(function () {
      return document.styleSheets;
    }, null);
    var ruleCount = 0;
    for (var s = 0; sheetList && s < sheetList.length; s++) {
      var some = safe(function () {
        return sheetList[s].cssRules;
      }, null);
      ruleCount += some ? some.length : 0;
    }
    held = {
      elements: safe(function () {
        return document.getElementsByTagName("*").length;
      }, 0),
      text: safe(function () {
        return document.body ? String(document.body.textContent || "").length : 0;
      }, 0),
      images: safe(function () {
        return document.images ? document.images.length : 0;
      }, 0),
      scripts: safe(function () {
        return document.scripts ? document.scripts.length : 0;
      }, 0),
      stylesheets: sheetList ? sheetList.length : 0,
      rules: ruleCount
    };
    heldAt = changed;
    return held;
  }

  /// What the page cost to arrive at: the stages of its own loading, when it
  /// first painted, and how much of everything it now holds.
  function timings() {
    var phases = [];
    var navigation = safe(function () {
      return performance.getEntriesByType("navigation")[0];
    }, null);
    var round = function (value) {
      return Math.round((value || 0) * 100) / 100;
    };
    if (navigation) {
      phases = [
        ["redirect", round(navigation.redirectEnd - navigation.redirectStart)],
        ["dns", round(navigation.domainLookupEnd - navigation.domainLookupStart)],
        ["connect", round(navigation.connectEnd - navigation.connectStart)],
        ["request", round(navigation.responseStart - navigation.requestStart)],
        ["response", round(navigation.responseEnd - navigation.responseStart)],
        ["dom", round(navigation.domContentLoadedEventEnd - navigation.responseEnd)],
        ["load", round(navigation.loadEventEnd - navigation.domContentLoadedEventEnd)]
      ];
      // Only when the engine has one to give: Servo answers nothing here, and a
      // total of nothing beside a page that took a third of a second is a lie.
      var whole = round(navigation.duration) ||
        round(navigation.loadEventEnd - navigation.startTime);
      if (whole > 0) {
        phases.push(["all of it", whole]);
      }
    } else {
      var legacy = safe(function () {
        return performance.timing;
      }, null);
      if (legacy && legacy.navigationStart) {
        var since = function (mark) {
          return legacy[mark] ? legacy[mark] - legacy.navigationStart : 0;
        };
        phases = [
          ["to first byte", since("responseStart")],
          ["response", since("responseEnd") - since("responseStart")],
          ["dom", since("domContentLoadedEventEnd") - since("responseEnd")],
          ["load", since("loadEventEnd") - since("domContentLoadedEventEnd")],
          ["all of it", since("loadEventEnd")]
        ];
      }
    }
    var paints = [];
    safe(function () {
      var entries = performance.getEntriesByType("paint") || [];
      for (var i = 0; i < entries.length; i++) {
        paints.push([entries[i].name, Math.round(entries[i].startTime)]);
      }
    });
    var resources = safe(function () {
      return performance.getEntriesByType("resource") || [];
    }, []);
    var transferred = 0;
    for (var r = 0; r < resources.length; r++) {
      transferred += resources[r].transferSize || resources[r].encodedBodySize || 0;
    }
    var held = whatItHolds();
    return json({
      phases: phases,
      paints: paints,
      counts: {
        elements: held.elements,
        text: held.text,
        images: held.images,
        scripts: held.scripts,
        stylesheets: held.stylesheets,
        rules: held.rules,
        listeners: listenerCount,
        requests: resources.length,
        transferred: transferred
      },
      memory: safe(function () {
        return performance.memory
          ? { used: performance.memory.usedJSHeapSize, total: performance.memory.totalJSHeapSize }
          : null;
      }, null)
    });
  }

  // ------------------------------------------------- what stands in a reader's way

  var NAMED = {
    black: [0, 0, 0],
    white: [255, 255, 255],
    red: [255, 0, 0],
    green: [0, 128, 0],
    blue: [0, 0, 255],
    gray: [128, 128, 128],
    grey: [128, 128, 128],
    silver: [192, 192, 192],
    transparent: [0, 0, 0]
  };

  /// A colour out of whatever the engine writes one as. Engines do not agree:
  /// commas or spaces between the channels, a slash before the alpha, three or
  /// six digits of hex, or a name. A colour this cannot read means no contrast
  /// can be worked out for that element, so it is worth reading all of them.
  function colourOf(text) {
    text = String(text || "").trim();
    if (!text) {
      return null;
    }
    var word = text.toLowerCase();
    if (NAMED[word]) {
      return {
        r: NAMED[word][0],
        g: NAMED[word][1],
        b: NAMED[word][2],
        a: word === "transparent" ? 0 : 1
      };
    }
    if (word.charAt(0) === "#") {
      var digits = word.slice(1);
      if (digits.length === 3 || digits.length === 4) {
        digits = digits.replace(/./g, function (one) {
          return one + one;
        });
      }
      if (digits.length !== 6 && digits.length !== 8) {
        return null;
      }
      var channel = function (at) {
        return parseInt(digits.substr(at, 2), 16);
      };
      return {
        r: channel(0),
        g: channel(2),
        b: channel(4),
        a: digits.length === 8 ? channel(6) / 255 : 1
      };
    }
    var inside = text.match(/rgba?\(([^)]+)\)/);
    if (!inside) {
      return null;
    }
    var parts = inside[1]
      .replace(/\//g, " ")
      .split(/[\s,]+/)
      .filter(function (piece) {
        return piece.length > 0;
      })
      .map(function (piece) {
        // A channel written as a percentage is still a channel.
        return piece.indexOf("%") >= 0
          ? (parseFloat(piece) / 100) * 255
          : parseFloat(piece);
      });
    if (parts.length < 3 || parts.slice(0, 3).some(isNaN)) {
      return null;
    }
    var alpha = parts.length > 3 && !isNaN(parts[3]) ? parts[3] : 1;
    // An alpha written as a percentage came out of the scaling above.
    if (alpha > 1) {
      alpha = alpha / 255;
    }
    return { r: parts[0], g: parts[1], b: parts[2], a: alpha };
  }

  /// One colour laid over another, which is what a reader actually sees.
  function over(top, bottom) {
    var alpha = Math.max(0, Math.min(1, top.a));
    return {
      r: top.r * alpha + bottom.r * (1 - alpha),
      g: top.g * alpha + bottom.g * (1 - alpha),
      b: top.b * alpha + bottom.b * (1 - alpha),
      a: 1
    };
  }

  function luminance(colour) {
    var channel = function (value) {
      value = value / 255;
      return value <= 0.03928 ? value / 12.92 : Math.pow((value + 0.055) / 1.055, 2.4);
    };
    return 0.2126 * channel(colour.r) + 0.7152 * channel(colour.g) + 0.0722 * channel(colour.b);
  }

  /// What is behind an element's words, which is every background from the
  /// element outwards laid over the next until one of them is opaque, and the
  /// white of the canvas under all of them.
  function behind(node) {
    var layers = [];
    while (node && node.nodeType === 1 && layers.length < 32) {
      var style = safe(function () {
        return window.getComputedStyle(node);
      }, null);
      var colour = style ? colourOf(style.getPropertyValue("background-color")) : null;
      if (colour && colour.a > 0) {
        layers.push(colour);
        if (colour.a >= 0.999) {
          break;
        }
      }
      node = node.parentNode;
    }
    var ground = { r: 255, g: 255, b: 255, a: 1 };
    for (var i = layers.length - 1; i >= 0; i--) {
      ground = over(layers[i], ground);
    }
    return ground;
  }

  function contrastOf(node) {
    var style = safe(function () {
      return window.getComputedStyle(node);
    }, null);
    if (!style) {
      return null;
    }
    var ink = colourOf(style.getPropertyValue("color"));
    if (!ink) {
      return null;
    }
    var ground = behind(node);
    // Text that is not opaque is the colour it comes out as over that ground,
    // not the colour it was asked for.
    var painted = ink.a < 0.999 ? over(ink, ground) : ink;
    var one = luminance(painted);
    var other = luminance(ground);
    var ratio = (Math.max(one, other) + 0.05) / (Math.min(one, other) + 0.05);
    var size = parseFloat(style.getPropertyValue("font-size")) || 16;
    var weight = parseInt(style.getPropertyValue("font-weight"), 10) || 400;
    var large = size >= 24 || (size >= 18.66 && weight >= 700);
    return {
      ratio: Math.round(ratio * 100) / 100,
      needs: large ? 3 : 4.5,
      large: large
    };
  }

  /// A named text for an element, the way a reader's screen reader would find
  /// one. This is the ordinary way round -- label, aria, title, own words -- not
  /// the full accessible-name algorithm, and it is enough to say when there is
  /// nothing at all.
  function namedBy(node) {
    var labelled = node.getAttribute("aria-label");
    if (labelled && labelled.trim()) {
      return "aria-label";
    }
    var by = node.getAttribute("aria-labelledby");
    if (by && document.getElementById(by.split(/\s+/)[0])) {
      return "aria-labelledby";
    }
    if (node.id && document.querySelector('label[for="' + node.id.replace(/"/g, '\\"') + '"]')) {
      return "label";
    }
    if (node.closest && node.closest("label")) {
      return "label";
    }
    var title = node.getAttribute("title");
    if (title && title.trim()) {
      return "title";
    }
    if (String(node.textContent || "").trim()) {
      return "its own words";
    }
    var image = node.querySelector ? node.querySelector("img[alt]") : null;
    if (image && String(image.getAttribute("alt") || "").trim()) {
      return "an image's alt";
    }
    if (node.getAttribute("alt") && node.getAttribute("alt").trim()) {
      return "alt";
    }
    return "";
  }

  /// How one element's text stands out from what is behind it, and what it owes
  /// the reader. Nothing back means the colours could not be read.
  function contrast(at) {
    var node = nodeAt(at);
    if (!node) {
      return "{}";
    }
    var own = ownWords(node);
    var measured = contrastOf(node);
    return json({
      words: cut(own, 60),
      ratio: measured ? measured.ratio : null,
      needs: measured ? measured.needs : null,
      large: measured ? measured.large : null
    });
  }

  /// The words an element says itself, rather than through its children: what is
  /// painted in its own colour, and so what its contrast is about.
  ///
  /// Our own selection wraps every word of the page in a span of ours, which
  /// leaves an element with nothing but the spaces between them. Those words are
  /// still the element's own and still painted in its colour, so they count --
  /// otherwise the reading of a page would stop finding text too faint to read
  /// the moment the reader selected anything.
  function ownWords(node) {
    var own = "";
    for (var i = 0; i < node.childNodes.length; i++) {
      var child = node.childNodes[i];
      if (child.nodeType === 3) {
        own += child.data;
      } else if (child.nodeType === 1 && child.getAttribute(MARK) === "word") {
        own += child.textContent || "";
      }
    }
    return own;
  }

  var CONTRAST_SAMPLE = 400;

  /// What would stand between this page and a reader who cannot see it, or
  /// cannot use a mouse. Every finding names the element it is about, so the
  /// panel can take the reader to it.
  function audit() {
    walkTree(40);
    var found = [];
    var say = function (level, rule, text, node) {
      found.push({
        level: level,
        rule: rule,
        text: cut(text, 300),
        at: node ? numbered.indexOf(node) : -1,
        selector: node ? selectorOf(node) : ""
      });
    };
    safe(function () {
      if (!document.documentElement.getAttribute("lang")) {
        say("warn", "language", "The page does not say what language it is in.", document.documentElement);
      }
    });
    safe(function () {
      if (!String(document.title || "").trim()) {
        say("warn", "title", "The page has no title.", null);
      }
    });
    var seenIds = {};
    for (var i = 0; i < numbered.length; i++) {
      var node = numbered[i];
      var name = node.nodeName.toLowerCase();
      var id = node.id;
      if (id) {
        if (seenIds[id]) {
          say("error", "duplicate id", 'More than one element has the id "' + id + '".', node);
        }
        seenIds[id] = true;
      }
      if (name === "img" && node.getAttribute("alt") === null) {
        say("error", "image without alt", "An image says nothing to a reader who cannot see it.", node);
      }
      if (name === "iframe" && !node.getAttribute("title")) {
        say("warn", "frame without title", "An embedded page has no title.", node);
      }
      if ((name === "a" && node.getAttribute("href") !== null) || name === "button" ||
          node.getAttribute("role") === "button" || node.getAttribute("role") === "link") {
        if (!namedBy(node)) {
          say("error", "nothing to read", "A " + name + " has no text and no label.", node);
        }
      }
      if (name === "input" || name === "select" || name === "textarea") {
        var kind = String(node.getAttribute("type") || "").toLowerCase();
        if (kind !== "hidden" && kind !== "submit" && kind !== "button" && kind !== "image" && !namedBy(node)) {
          var placeholder = node.getAttribute("placeholder");
          say(
            placeholder ? "warn" : "error",
            "field without a label",
            placeholder
              ? "A field is named only by its placeholder, which is gone as soon as it is typed in."
              : "A field has no label.",
            node
          );
        }
      }
      var tabIndex = node.getAttribute("tabindex");
      if (tabIndex && parseInt(tabIndex, 10) > 0) {
        say("warn", "tab order", "A tabindex above zero takes this element out of the page's own order.", node);
      }
      if (/^h[1-6]$/.test(name) && !String(node.textContent || "").trim()) {
        say("warn", "empty heading", "A heading with no words in it.", node);
      }
      if (node.getAttribute("aria-hidden") === "true" &&
          node.querySelector && node.querySelector("a[href],button,input,select,textarea,[tabindex]")) {
        say("error", "hidden but reachable", "Something hidden from a screen reader can still be tabbed to.", node);
      }
    }
    safe(function () {
      var headings = document.querySelectorAll("h1,h2,h3,h4,h5,h6");
      var was = 0;
      for (var h = 0; h < headings.length; h++) {
        var level = parseInt(headings[h].nodeName.slice(1), 10);
        if (was && level > was + 1) {
          say("warn", "heading order", "The page jumps from h" + was + " to h" + level + ".", headings[h]);
        }
        was = level;
      }
    });
    // Contrast, on the elements that actually carry words. A page can have
    // thousands, so a bounded sample is taken rather than all of them.
    var checked = 0;
    for (var c = 0; c < numbered.length && checked < CONTRAST_SAMPLE; c++) {
      var element = numbered[c];
      if (!ownWords(element).trim()) {
        continue;
      }
      checked++;
      var measured = contrastOf(element);
      if (measured && measured.ratio < measured.needs) {
        say(
          "error",
          "hard to read",
          "Text at " + measured.ratio + ":1 against what is behind it, where " +
            measured.needs + ":1 is the least that passes.",
          element
        );
      }
    }
    return json(found);
  }

  /// The order the keyboard walks the page in, drawn on the page itself.
  var numbers = [];

  function tabOrder(on) {
    for (var i = 0; i < numbers.length; i++) {
      if (numbers[i].parentNode) {
        numbers[i].parentNode.removeChild(numbers[i]);
      }
    }
    numbers = [];
    if (String(on) === "0" || String(on) === "false") {
      return "off";
    }
    var focusable = safe(function () {
      return document.querySelectorAll(
        "a[href],button,input,select,textarea,[tabindex]:not([tabindex='-1'])"
      );
    }, []);
    var order = [];
    for (var f = 0; f < focusable.length; f++) {
      var node = focusable[f];
      if (node.disabled || ours(node)) {
        continue;
      }
      order.push({ node: node, index: parseInt(node.getAttribute("tabindex") || "0", 10), was: f });
    }
    order.sort(function (left, right) {
      if (left.index > 0 && right.index <= 0) {
        return -1;
      }
      if (right.index > 0 && left.index <= 0) {
        return 1;
      }
      if (left.index > 0 && right.index > 0 && left.index !== right.index) {
        return left.index - right.index;
      }
      return left.was - right.was;
    });
    for (var o = 0; o < order.length && o < 200; o++) {
      var box = order[o].node.getBoundingClientRect();
      var tag = overlay("tab-order");
      tag.style.cssText +=
        ";background:rgba(220,80,40,0.95);color:#fff;font:11px/14px monospace;padding:0 3px";
      tag.style.left = Math.max(0, box.left) + "px";
      tag.style.top = Math.max(0, box.top) + "px";
      tag.textContent = String(o + 1);
      numbers.push(tag);
    }
    return String(numbers.length);
  }

  // ----------------------------------------------------------- rulers and measure

  var ruler = null;

  /// Lines across the page every hundred pixels, for reading a layout off the
  /// page itself.
  function rulers(on) {
    if (ruler && ruler.parentNode) {
      ruler.parentNode.removeChild(ruler);
    }
    ruler = null;
    if (String(on) === "0" || String(on) === "false") {
      return "off";
    }
    ruler = overlay("rulers");
    ruler.style.cssText +=
      ";left:0;top:0;right:0;bottom:0;" +
      "background-image:" +
      "linear-gradient(to right, rgba(64,138,240,0.35) 0 1px, transparent 1px 100%)," +
      "linear-gradient(to bottom, rgba(64,138,240,0.35) 0 1px, transparent 1px 100%)," +
      "linear-gradient(to right, rgba(64,138,240,0.12) 0 1px, transparent 1px 100%)," +
      "linear-gradient(to bottom, rgba(64,138,240,0.12) 0 1px, transparent 1px 100%);" +
      "background-size:100px 100px,100px 100px,10px 10px,10px 10px";
    return "on";
  }

  var measuring = false;
  var measureFrom = null;
  var measureBox = null;

  addListener.call(window, "mousedown", function (event) {
    if (picking) {
      // Held back here as well as on the click: the page's own selection
      // starts on the press, and picking an element must not drag over it.
      event.preventDefault();
      event.stopPropagation();
      if (event.stopImmediatePropagation) {
        event.stopImmediatePropagation();
      }
      return;
    }
    if (!measuring) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    measureFrom = { x: event.clientX, y: event.clientY };
  }, true);

  addListener.call(window, "mousemove", function (event) {
    if (!measuring || !measureFrom) {
      return;
    }
    if (!measureBox) {
      measureBox = overlay("measure");
      measureBox.style.cssText +=
        ";border:1px solid rgba(220,80,40,0.95);background:rgba(220,80,40,0.15);" +
        "color:#fff;font:11px/14px monospace";
    }
    var left = Math.min(measureFrom.x, event.clientX);
    var top = Math.min(measureFrom.y, event.clientY);
    var width = Math.abs(event.clientX - measureFrom.x);
    var height = Math.abs(event.clientY - measureFrom.y);
    measureBox.style.display = "block";
    measureBox.style.left = left + "px";
    measureBox.style.top = top + "px";
    measureBox.style.width = width + "px";
    measureBox.style.height = height + "px";
    measureBox.textContent = width + "×" + height;
  }, true);

  addListener.call(window, "mouseup", function () {
    if (measuring) {
      measureFrom = null;
    }
  }, true);

  /// A ruler the reader drags across the page.
  function measure(on) {
    measuring = String(on) !== "0" && String(on) !== "false";
    if (!measuring) {
      measureFrom = null;
      if (measureBox) {
        measureBox.style.display = "none";
      }
    }
    return measuring ? "on" : "off";
  }

  // ------------------------------------------------------------------ the console's

  window.$0 = null;
  window.$_ = null;
  if (typeof window.$ === "undefined") {
    window.$ = function (query) {
      return document.querySelector(query);
    };
  }
  if (typeof window.$$ === "undefined") {
    window.$$ = function (query) {
      return [].slice.call(document.querySelectorAll(query));
    };
  }

  /// Runs what the reader typed and describes what it evaluated to. The value is
  /// kept as `$_`, the way a console keeps the last answer.
  function run(script) {
    var value;
    try {
      // Indirect, so that what is typed runs in the page's own scope rather
      // than inside this function.
      var indirect = window.eval;
      value = indirect(String(script));
    } catch (error) {
      return "!!" + (error && error.name ? error.name + ": " + error.message : String(error));
    }
    window.$_ = value;
    return describeDeeply(value);
  }

  /// Which element the reader has picked, for `$0` and for the panel's actions.
  function chose(at) {
    var node = nodeAt(at);
    if (node) {
      window.$0 = node;
    }
    return node ? "yes" : "";
  }

  // A name for this document, so a panel holding a number from the last one can
  // tell that it no longer means anything. Made from the numbers the page itself
  // can give, since a fresh install is a fresh document.
  var thisDocument = "d" + Math.floor(Math.random() * 1e9) + "-" + performance.now();

  window.__zedTools = {
    who: function () {
      return thisDocument;
    },
    said: function () {
      var out = json(said);
      said = [];
      return out;
    },
    tree: tree,
    treeIfChanged: treeIfChanged,
    path: path,
    about: about,
    rules: rules,
    computed: computed,
    layout: layout,
    listening: listening,
    fonts: fonts,
    installed: installed,
    selector: selector,
    html: html,
    setHtml: setHtml,
    bring: bring,
    remove: remove,
    highlight: highlight,
    pick: pick,
    picked: picked,
    chose: chose,
    fetched: network,
    network: network,
    request: request,
    sheets: sheets,
    sheet: sheet,
    toggleSheet: toggleSheet,
    storage: storage,
    forget: forget,
    clearStore: clearStore,
    timings: timings,
    audit: audit,
    contrast: contrast,
    tabOrder: tabOrder,
    rulers: rulers,
    measure: measure,
    run: run
  };
})();
