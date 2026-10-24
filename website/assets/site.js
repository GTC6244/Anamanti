// Anamanti website: mobile nav toggle, click-to-enlarge screenshots, and the
// waitlist / contact forms (POST JSON to the Worker's /api/* endpoints).
// Progressive enhancement only; every page works without it.
(function () {
  var header = document.querySelector('.site-header');
  var toggle = document.querySelector('.nav-toggle');
  if (header && toggle) {
    toggle.addEventListener('click', function () {
      var open = header.classList.toggle('open');
      toggle.setAttribute('aria-expanded', open ? 'true' : 'false');
    });
  }

  var box = document.createElement('div');
  box.className = 'lightbox';
  box.setAttribute('role', 'dialog');
  box.setAttribute('aria-modal', 'true');
  box.innerHTML = '<figure><img alt=""><p></p></figure>';
  document.body.appendChild(box);
  var big = box.querySelector('img');
  var cap = box.querySelector('p');

  function close() { box.classList.remove('open'); big.removeAttribute('src'); }
  box.addEventListener('click', close);
  document.addEventListener('keydown', function (e) { if (e.key === 'Escape') close(); });

  document.querySelectorAll('img.zoom').forEach(function (img) {
    img.tabIndex = 0;
    function open() {
      big.src = img.currentSrc || img.src;
      big.alt = img.alt;
      var fc = img.closest('figure') && img.closest('figure').querySelector('figcaption');
      cap.textContent = fc ? fc.textContent : img.alt;
      box.classList.add('open');
    }
    img.addEventListener('click', open);
    img.addEventListener('keydown', function (e) { if (e.key === 'Enter') open(); });
  });

  var MESSAGES = {
    email: 'Please enter a valid email address.',
    message: 'Please write a message.',
    rate: 'Too many submissions from your connection. Please try again in an hour.',
  };
  document.querySelectorAll('form[data-endpoint]').forEach(function (form) {
    var status = form.querySelector('.form-status');
    var button = form.querySelector('button[type="submit"]');
    form.addEventListener('submit', function (e) {
      e.preventDefault();
      var data = {};
      new FormData(form).forEach(function (v, k) { data[k] = v; });
      status.className = 'form-status';
      status.textContent = 'Sending…';
      button.disabled = true;
      fetch(form.getAttribute('data-endpoint'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(data),
      }).then(function (r) { return r.json().catch(function () { return { ok: false }; }); })
        .then(function (res) {
          if (res.ok) {
            form.reset();
            status.className = 'form-status ok';
            status.textContent = form.id === 'waitlist'
              ? "You're on the list. We'll be in touch."
              : 'Thanks, your message is on its way. We will reply by email.';
          } else {
            status.className = 'form-status err';
            status.textContent = MESSAGES[res.error] || 'Something went wrong. Please try again.';
          }
        })
        .catch(function () {
          status.className = 'form-status err';
          status.textContent = 'Could not reach the server. Please try again.';
        })
        .finally(function () { button.disabled = false; });
    });
  });
})();
