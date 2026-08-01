// Selecting text with the mouse, done in the page because the engine cannot do
// it. Servo builds a paintable selection only for <input> and <textarea>, it has
// no caretPositionFromPoint, and -- measured, not assumed -- its Range geometry
// returns nothing at all: getClientRects() gives zero rectangles and
// getBoundingClientRect() gives zeroes. Element geometry, on the other hand,
// works.
//
// So every word is wrapped in a span once, on the first drag, and those spans
// are what the pointer is matched against; the character within a word comes
// from interpolating across the word's own box. The highlight is drawn as
// translucent boxes in an overlay, so the page's text is never restyled, and the
// selected string is handed to the editor through __zedSelection.text().
(function () {
  if (window.__zedSelection) {
    return;
  }

  var HIGHLIGHT = "rgba(64, 138, 240, 0.35)";
  var SKIP = { SCRIPT: 1, STYLE: 1, TEXTAREA: 1, INPUT: 1, SELECT: 1, NOSCRIPT: 1 };
  // A ceiling on how much of a page is prepared for selection. Every word costs
  // an element in the layout, and a document of a hundred thousand words would
  // pay for selection it will never use.
  var MOST_WORDS = 20000;

  var words = null;
  var measured = null;
  var measuredAt = { x: 0, y: 0 };
  var overlay = null;
  var anchor = null;
  var head = null;
  var dragging = false;

  function overlayHost() {
    if (overlay && overlay.parentNode) {
      return overlay;
    }
    overlay = document.createElement("div");
    overlay.setAttribute("data-zed-selection", "overlay");
    overlay.style.cssText =
      "position:fixed;left:0;top:0;right:0;bottom:0;pointer-events:none;z-index:2147483647";
    document.documentElement.appendChild(overlay);
    return overlay;
  }

  function clearHighlight() {
    if (overlay) {
      overlay.textContent = "";
    }
  }

  // Every word gets a span of its own, so that there is something with a
  // measurable box for each run of text. Whitespace is left as it was, so the
  // page's own layout does not shift.
  function wrapWords() {
    if (words) {
      return words;
    }
    words = [];
    var wrapped = 0;
    var texts = [];
    var walker = document.createTreeWalker(document.body, 4, null);
    var node;
    while ((node = walker.nextNode())) {
      if (!node.data || !node.data.trim()) {
        continue;
      }
      var parent = node.parentNode;
      if (!parent || SKIP[parent.nodeName] || parent.getAttribute("data-zed-selection")) {
        continue;
      }
      texts.push(node);
    }
    for (var i = 0; i < texts.length; i++) {
      var text = texts[i];
      var pieces = text.data.split(/(\s+)/);
      var replacement = document.createDocumentFragment();
      var made = [];
      for (var p = 0; p < pieces.length; p++) {
        var piece = pieces[p];
        if (!piece) {
          continue;
        }
        if (/^\s+$/.test(piece)) {
          replacement.appendChild(document.createTextNode(piece));
          continue;
        }
        var span = document.createElement("span");
        span.setAttribute("data-zed-selection", "word");
        span.textContent = piece;
        replacement.appendChild(span);
        made.push(span);
      }
      if (!made.length) {
        continue;
      }
      text.parentNode.replaceChild(replacement, text);
      wrapped += made.length;
      if (wrapped >= MOST_WORDS) {
        break;
      }
    }
    // Taken from the page rather than from what was just wrapped: a page that
    // has been through this before still carries its spans, and this run skips
    // them. Reading them back is what keeps a page that rewrote itself -- and so
    // was wrapped again -- from ending up with no words at all.
    var already = document.querySelectorAll('[data-zed-selection="word"]');
    for (var w = 0; w < already.length && w < MOST_WORDS; w++) {
      words.push({ span: already[w], text: already[w].textContent });
    }
    // Reading a geometry property makes the engine lay the page out again, so
    // what is measured afterwards is the page as it now is rather than as it was
    // before the spans went in.
    void document.body.offsetHeight;
    measured = null;
    return words;
  }

  // Every word's box, measured in one pass and kept until the page moves. A
  // measurement is a layout query, and asking for one per word per mouse move --
  // hundreds of them, thirty times a second -- costs far more than the drag.
  // Every word's box, measured in one pass and kept until the page changes. The
  // boxes are in viewport coordinates, so a scroll moves the text out from under
  // them -- but it moves every word by the same amount, so the boxes are shifted
  // rather than measured again. Measuring is what a scroll used to cost, and it
  // cost it in the page's own script, on every turn of the wheel.
  function boxes() {
    var all = wrapWords();
    var x = window.scrollX;
    var y = window.scrollY;
    if (measured && measured.length === all.length) {
      var acrossBy = measuredAt.x - x;
      var downBy = measuredAt.y - y;
      if (acrossBy !== 0 || downBy !== 0) {
        for (var moved = 0; moved < measured.length; moved++) {
          var box = measured[moved];
          measured[moved] = {
            left: box.left + acrossBy,
            right: box.right + acrossBy,
            top: box.top + downBy,
            bottom: box.bottom + downBy,
            width: box.width,
            height: box.height,
          };
        }
        measuredAt = { x: x, y: y };
      }
      return measured;
    }
    measured = [];
    measuredAt = { x: x, y: y };
    for (var i = 0; i < all.length; i++) {
      var rectangle = all[i].span.getBoundingClientRect();
      measured.push({
        left: rectangle.left,
        right: rectangle.right,
        top: rectangle.top,
        bottom: rectangle.bottom,
        width: rectangle.width,
        height: rectangle.height,
      });
    }
    return measured;
  }

  function boxOf(index) {
    return boxes()[index];
  }

  // The word at the point, or the closest one to it. Closest matters more than it
  // sounds: a word's box is the height of its glyphs, not of its line, so with
  // any line spacing at all a good part of the page falls between boxes -- and a
  // reader who starts a drag there means the line they aimed at.
  function wordAt(x, y) {
    var all = boxes();
    var onTheLine = null;
    var onTheLineDistance = Infinity;
    var nearest = null;
    var nearestVertical = Infinity;
    var nearestHorizontal = Infinity;
    for (var i = 0; i < all.length; i++) {
      var box = all[i];
      if (box.height <= 0) {
        continue;
      }
      var vertical = y < box.top ? box.top - y : y > box.bottom ? y - box.bottom : 0;
      var horizontal = x < box.left ? box.left - x : x > box.right ? x - box.right : 0;
      if (vertical === 0) {
        if (horizontal === 0) {
          return { index: i, box: box };
        }
        if (horizontal < onTheLineDistance) {
          onTheLine = { index: i, box: box };
          onTheLineDistance = horizontal;
        }
        continue;
      }
      if (
        vertical < nearestVertical ||
        (vertical === nearestVertical && horizontal < nearestHorizontal)
      ) {
        nearest = { index: i, box: box };
        nearestVertical = vertical;
        nearestHorizontal = horizontal;
      }
    }
    return onTheLine || nearest;
  }

  // Which character of the word the pointer is nearest, by dividing the word's
  // own box evenly: the engine will not measure a single character for us.
  function characterIn(word, box, x) {
    var length = word.text.length;
    if (length < 1 || box.width <= 0) {
      return 0;
    }
    var across = (x - box.left) / box.width;
    return Math.max(0, Math.min(length, Math.round(across * length)));
  }

  function placeAt(x, y) {
    var found = wordAt(x, y);
    if (!found) {
      return null;
    }
    var word = words[found.index];
    return {
      index: found.index,
      offset: characterIn(word, found.box, x),
    };
  }

  function ordered() {
    if (!anchor || !head) {
      return null;
    }
    var first = anchor;
    var last = head;
    if (last.index < first.index || (last.index === first.index && last.offset < first.offset)) {
      first = head;
      last = anchor;
    }
    if (first.index === last.index && first.offset === last.offset) {
      return null;
    }
    return { first: first, last: last };
  }

  function paintBox(host, left, top, width, height) {
    if (width <= 0 || height <= 0) {
      return;
    }
    var box = document.createElement("div");
    box.style.cssText =
      "position:fixed;background:" +
      HIGHLIGHT +
      ";left:" + left + "px;top:" + top + "px;width:" + width + "px;height:" + height + "px";
    host.appendChild(box);
  }

  // One box per line rather than one per word: it looks like a selection instead
  // of a row of stripes, and a page-long drag costs a handful of elements rather
  // than thousands of them on every mouse move.
  function paintHighlight() {
    clearHighlight();
    var span = ordered();
    followTheScroll(!!span);
    if (!span) {
      return;
    }
    var host = overlayHost();
    var height = window.innerHeight || 0;
    var line = null;
    var flush = function () {
      if (line) {
        paintBox(host, line.left, line.top, line.right - line.left, line.bottom - line.top);
        line = null;
      }
    };
    for (var i = span.first.index; i <= span.last.index; i++) {
      var word = words[i];
      var box = boxOf(i);
      if (box.width <= 0 || box.height <= 0) {
        continue;
      }
      // Off-screen lines are not drawn; a scroll redraws what comes into view.
      if (box.bottom < 0 || (height && box.top > height)) {
        flush();
        continue;
      }
      var perCharacter = box.width / Math.max(1, word.text.length);
      var from = i === span.first.index ? span.first.offset : 0;
      var to = i === span.last.index ? span.last.offset : word.text.length;
      var left = box.left + from * perCharacter;
      var right = box.left + to * perCharacter;
      if (line && Math.abs(line.top - box.top) <= 2) {
        line.left = Math.min(line.left, left);
        line.right = Math.max(line.right, right);
        line.bottom = Math.max(line.bottom, box.bottom);
      } else {
        flush();
        line = { top: box.top, bottom: box.bottom, left: left, right: right };
      }
    }
    flush();
  }

  function selectedText() {
    var span = ordered();
    if (!span) {
      return "";
    }
    var out = "";
    for (var i = span.first.index; i <= span.last.index; i++) {
      var word = words[i];
      var from = i === span.first.index ? span.first.offset : 0;
      var to = i === span.last.index ? span.last.offset : word.text.length;
      out += word.text.slice(from, to);
      if (i < span.last.index) {
        out += " ";
      }
    }
    return out;
  }

  // Whatever goes wrong in here is recorded rather than thrown: a listener that
  // dies takes selection with it and says nothing about why.
  function guarded(handler) {
    return function (event) {
      try {
        handler(event);
      } catch (error) {
        window.__zedSelectionError = String(error);
      }
    };
  }

  document.addEventListener(
    "mousedown",
    guarded(function (event) {
      if (event.button !== 0) {
        return;
      }
      dragging = true;
      head = null;
      clearHighlight();
      anchor = placeAt(event.clientX, event.clientY);
    }),
    true
  );

  document.addEventListener(
    "mousemove",
    guarded(function (event) {
      if (!dragging || !anchor) {
        return;
      }
      head = placeAt(event.clientX, event.clientY) || head;
      paintHighlight();
    }),
    true
  );

  document.addEventListener(
    "mouseup",
    guarded(function (event) {
      if (!dragging) {
        return;
      }
      dragging = false;
      if (anchor) {
        head = placeAt(event.clientX, event.clientY) || head;
        paintHighlight();
      }
    }),
    true
  );

  // A double click takes the whole word, which is what a double click is for.
  document.addEventListener(
    "dblclick",
    guarded(function (event) {
      var found = wordAt(event.clientX, event.clientY);
      if (!found) {
        return;
      }
      anchor = { index: found.index, offset: 0 };
      head = { index: found.index, offset: words[found.index].text.length };
      paintHighlight();
    }),
    true
  );

  // A resize lays the page out afresh, so what was measured no longer describes
  // it.
  var relaidOut = guarded(function () {
    measured = null;
    if (ordered()) {
      paintHighlight();
    }
  });
  window.addEventListener("resize", relaidOut, true);

  // The highlight is drawn in viewport coordinates, so it has to be drawn again
  // when the page scrolls out from under it. That listener is only attached
  // while there is something highlighted: a page with a scroll handler makes the
  // engine run script on every turn of the wheel, whether the handler has
  // anything to do or not, and most turns of the wheel happen with nothing
  // selected at all.
  var scrolled = guarded(function () {
    paintHighlight();
  });
  var listeningToScroll = false;
  function followTheScroll(should) {
    if (should === listeningToScroll) {
      return;
    }
    listeningToScroll = should;
    if (should) {
      window.addEventListener("scroll", scrolled, true);
    } else {
      window.removeEventListener("scroll", scrolled, true);
    }
  }

  // A page that rewrites itself leaves the word list describing a document that
  // is no longer there, so the list is thrown away and built again once the page
  // has settled. Our own overlay and word spans are ignored, or this would never
  // stop rebuilding.
  var rebuilding = null;
  function rebuildLater() {
    if (rebuilding) {
      clearTimeout(rebuilding);
    }
    rebuilding = setTimeout(
      guarded(function () {
        rebuilding = null;
        words = null;
        measured = null;
        anchor = null;
        head = null;
        clearHighlight();
        wrapWords();
      }),
      250
    );
  }

  function watchForChanges() {
    if (typeof MutationObserver !== "function") {
      return;
    }
    var observer = new MutationObserver(function (records) {
      for (var i = 0; i < records.length; i++) {
        var target = records[i].target;
        var ours =
          target &&
          target.parentElement &&
          target.parentElement.getAttribute &&
          target.parentElement.getAttribute("data-zed-selection");
        if (!ours) {
          rebuildLater();
          return;
        }
      }
    });
    observer.observe(document.body, { childList: true, subtree: true, characterData: true });
  }

  // The words are wrapped ahead of the first drag, because measuring boxes in the
  // same turn as the wrapping gives zeroes and the drag would select nothing. But
  // not while the page is still arriving: the wrapping is put off until after the
  // page has been shown, so what the reader waits for is the page, not this.
  var prepare = guarded(function () {
    setTimeout(
      guarded(function () {
        wrapWords();
        watchForChanges();
      }),
      250
    );
  });
  if (document.readyState === "complete") {
    prepare();
  } else {
    window.addEventListener("load", prepare, false);
  }

  window.__zedSelection = {
    text: selectedText,
    // What the two ends of the selection are, for working out why a drag
    // picked what it picked.
    trace: function () {
      return JSON.stringify({
        anchor: anchor,
        head: head,
        words: words ? words.length : 0,
      });
    },
    clear: function () {
      anchor = null;
      head = null;
      clearHighlight();
    },
  };
})();
