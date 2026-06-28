/* Photon — print a single photo as a real 10×15 cm print (not a web-page chunk).
   Opens a hidden iframe containing ONLY the image with an @page rule sized to
   10×15 (portrait) or 15×10 (landscape) depending on the image, full-bleed, and
   triggers the browser print dialog once the image has loaded. */

/** Print `url` at 10×15 cm, oriented to match the photo. */
export function printPhoto(url: string, portrait: boolean): void {
  const pageSize = portrait ? '10cm 15cm' : '15cm 10cm';
  const w = portrait ? '10cm' : '15cm';
  const h = portrait ? '15cm' : '10cm';
  const html = `<!doctype html><html><head><meta charset="utf-8">
<style>
  @page { size: ${pageSize}; margin: 0; }
  html, body { margin: 0; padding: 0; }
  /* Full-bleed 10×15 print; a 3:2 photo fills it exactly, others are centre-cropped. */
  .sheet { width: ${w}; height: ${h}; overflow: hidden; }
  img { width: 100%; height: 100%; object-fit: cover; display: block; }
  @media print { -webkit-print-color-adjust: exact; print-color-adjust: exact; }
</style></head>
<body><div class="sheet"><img src="${url}" alt=""></div></body></html>`;

  const iframe = document.createElement('iframe');
  iframe.setAttribute('aria-hidden', 'true');
  iframe.style.cssText = 'position:fixed;right:0;bottom:0;width:0;height:0;border:0;';
  document.body.appendChild(iframe);

  const cleanup = () => {
    setTimeout(() => iframe.remove(), 1000);
  };

  const doc = iframe.contentWindow!.document;
  doc.open();
  doc.write(html);
  doc.close();

  const img = doc.querySelector('img')!;
  const go = () => {
    try {
      iframe.contentWindow!.focus();
      iframe.contentWindow!.onafterprint = cleanup;
      iframe.contentWindow!.print();
    } catch {
      cleanup();
    }
  };
  if (img.complete) setTimeout(go, 50);
  else {
    img.onload = () => setTimeout(go, 50);
    img.onerror = cleanup;
  }
  // Safety net if onafterprint never fires.
  setTimeout(cleanup, 60000);
}
