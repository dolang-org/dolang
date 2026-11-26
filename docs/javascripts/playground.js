// Moves each "Open in playground" link into its code block's button bar,
// beside the copy button, as an icon button. Without the bar (or without
// JavaScript) the link stays a text link under the block.
document$.subscribe(() => {
  for (const link of document.querySelectorAll(".playground-example > a.playground-open")) {
    const nav = link.parentElement.querySelector(".md-code__nav");
    if (!nav) continue;
    link.classList.add("md-code__button");
    link.dataset.mdType = "playground";
    link.title = link.textContent;
    link.setAttribute("aria-label", link.textContent);
    link.textContent = "";
    nav.prepend(link);
  }
});
