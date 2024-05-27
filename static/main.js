function homepage() {
  window.addEventListener("load", () => {
    const addButton = document.querySelector("#addOption");
    const options = document.querySelector("#options");

    if (!addButton || !options) return;

    setupDeletes();

    addButton.addEventListener("click", () => {
      options.appendChild(makeOption());
      setupDeletes();
    });
  });

  function makeOption() {
    const div = document.createElement("div");
    div.classList.add("flex");
    div.classList.add("gap-sm");

    div.innerHTML = `
    <input class="input-text strech" name="options" required="true" placeholder="a choice">
    <button class="button w-fit delete" type="button">DELETE</button>
    `;

    return div;
  }

  function setupDeletes() {
    const deletes = document.querySelectorAll(".delete");

    deletes.forEach(del => {
      del.addEventListener("click", () => {
        del.parentNode.remove();
      });
    });
  }
}

function setupSortable() {
  document.addEventListener("htmx:afterSettle", () => {
    const sortables = document.querySelectorAll(".sortable");

    for (const sortable of sortables) {
        new Sortable(sortable, {
            animation: 150,
            ghostClass: "sortable-active"
        });
    }
  });
}


homepage();
setupSortable();
