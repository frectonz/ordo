window.addEventListener("load", () => {
  const addButton = document.querySelector("#addOption");
  const options = document.querySelector(".options");

  setupDeletes();

  addButton.addEventListener("click", () => {
    options.appendChild(makeOption());
    setupDeletes();
  });
});

function makeOption() {
  const div = document.createElement("div");
  div.classList.add("option");

  div.innerHTML = `
  <input class="regular" name="options" required="true" placeholder="a choice"></input>
  <button class="bold delete" type="button">delete</button>
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
