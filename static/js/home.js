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
  <input class="hand" name="option" required="true" placeholder="a choice"></input>
  <button class="hand delete" type="button">delete</button>
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
