// Anamanti website: mobile nav toggle + click-to-enlarge screenshots.
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
})();
